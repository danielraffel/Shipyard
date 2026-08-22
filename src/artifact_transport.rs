//! Pure, default-off primitives for transferring immutable build artifacts.
//!
//! This module deliberately does not dispatch work or select hosts. A caller must
//! provide an exact manifest authority, an already-authorized staging root, and
//! a receiver-pull transport. Publication is fail-closed and digest addressed.
#![allow(missing_docs)]

use fs2::available_space;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

/// Current on-disk/wire manifest schema.
pub const MANIFEST_SCHEMA: u32 = 1;
/// Default chunk size for newly-created manifests.
pub const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
const MIN_CHUNK_SIZE: u64 = 64 * 1024;
const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// Artifact transport validation or publication failure.
#[derive(Debug)]
pub enum Error {
    /// A manifest or caller-supplied identifier is invalid.
    Invalid(String),
    /// The supplied authority does not authorize this exact artifact.
    StaleFence(String),
    /// Available disk space does not satisfy the configured reserve.
    InsufficientSpace { required: u64, available: u64 },
    /// Filesystem or stream operation failed.
    Io(std::io::Error),
    /// Canonical manifest serialization failed.
    Json(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::StaleFence(message) => formatter.write_str(message),
            Self::InsufficientSpace {
                required,
                available,
            } => write!(
                formatter,
                "artifact staging requires {required} bytes but only {available} are available"
            ),
            Self::Io(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

/// Exact source identity of the build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    /// Repository in `owner/name` form.
    pub repository: String,
    /// Exact commit built by the producer.
    pub head_sha: String,
    /// Exact Git tree built by the producer.
    pub tree_sha: String,
}

/// Exact toolchain and test-inventory identity used by the build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildIdentity {
    pub platform: String,
    pub architecture: String,
    pub build_type: String,
    pub toolchain_sha256: String,
    pub golden_image_sha256: Option<String>,
    pub test_inventory_sha256: String,
}

/// Supported immutable artifact encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactFormat {
    TarZstd,
}

/// A regular file or directory in the unpacked artifact layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LayoutEntry {
    Directory {
        path: String,
        mode: u32,
    },
    File {
        path: String,
        mode: u32,
        size_bytes: u64,
        sha256: String,
    },
}

impl LayoutEntry {
    fn path(&self) -> &str {
        match self {
            Self::Directory { path, .. } | Self::File { path, .. } => path,
        }
    }
}

/// One authenticated fixed-size chunk of the encoded artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactChunk {
    pub offset: u64,
    pub size_bytes: u64,
    pub sha256: String,
}

/// Immutable external cache generation required by the artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheGeneration {
    pub name: String,
    pub generation: String,
    pub sha256: String,
    pub required: bool,
}

/// Producer lease fence that prevents a stale worker from publishing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerFence {
    pub worker_id: String,
    pub lease_id: String,
    pub generation: u64,
    pub attempt: u32,
}

/// Complete immutable artifact description.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifest {
    pub schema: u32,
    pub source: SourceIdentity,
    pub build: BuildIdentity,
    pub format: ArtifactFormat,
    pub artifact_sha256: String,
    pub artifact_size_bytes: u64,
    pub chunk_size_bytes: u64,
    pub layout_sha256: String,
    pub entries: Vec<LayoutEntry>,
    pub chunks: Vec<ArtifactChunk>,
    pub cache_generations: Vec<CacheGeneration>,
    pub producer: ProducerFence,
}

/// Queue/lease authority for exactly one canonical manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestAuthority {
    pub manifest_sha256: String,
    pub repository: String,
    pub head_sha: String,
    pub tree_sha: String,
    pub worker_id: String,
    pub lease_id: String,
    pub generation: u64,
    pub attempt: u32,
}

impl ArtifactManifest {
    /// Validate structure, identity bindings, chunk coverage, and layout digest.
    pub fn validate(&self) -> Result<(), Error> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(Error::Invalid(format!(
                "unsupported manifest schema {}",
                self.schema
            )));
        }
        validate_repository(&self.source.repository)?;
        validate_git_oid(&self.source.head_sha, "head SHA")?;
        validate_git_oid(&self.source.tree_sha, "tree SHA")?;
        for (label, value) in [
            ("platform", self.build.platform.as_str()),
            ("architecture", self.build.architecture.as_str()),
            ("build type", self.build.build_type.as_str()),
            ("worker id", self.producer.worker_id.as_str()),
            ("lease id", self.producer.lease_id.as_str()),
        ] {
            validate_label(value, label)?;
        }
        validate_digest(&self.build.toolchain_sha256, "toolchain digest")?;
        validate_digest(&self.build.test_inventory_sha256, "test inventory digest")?;
        if let Some(digest) = &self.build.golden_image_sha256 {
            validate_digest(digest, "golden image digest")?;
        }
        validate_digest(&self.artifact_sha256, "artifact digest")?;
        validate_digest(&self.layout_sha256, "layout digest")?;
        if self.artifact_size_bytes == 0 {
            return Err(Error::Invalid("artifact must not be empty".into()));
        }
        if !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&self.chunk_size_bytes) {
            return Err(Error::Invalid(
                "chunk size is outside the supported range".into(),
            ));
        }
        self.validate_entries()?;
        self.validate_chunks()?;
        self.validate_caches()?;
        Ok(())
    }

    fn validate_entries(&self) -> Result<(), Error> {
        if self.entries.is_empty() {
            return Err(Error::Invalid("artifact layout must not be empty".into()));
        }
        let mut previous = None;
        for entry in &self.entries {
            validate_relative_path(entry.path())?;
            if previous.is_some_and(|value: &str| value >= entry.path()) {
                return Err(Error::Invalid(
                    "layout entries must be sorted and unique".into(),
                ));
            }
            previous = Some(entry.path());
            match entry {
                LayoutEntry::Directory { mode, .. } => validate_mode(*mode)?,
                LayoutEntry::File { mode, sha256, .. } => {
                    validate_mode(*mode)?;
                    validate_digest(sha256, "layout file digest")?;
                }
            }
        }
        let encoded = serde_json::to_vec(&self.entries)?;
        if sha256_bytes(&encoded) != self.layout_sha256 {
            return Err(Error::Invalid(
                "layout digest does not match entries".into(),
            ));
        }
        Ok(())
    }

    fn validate_chunks(&self) -> Result<(), Error> {
        if self.chunks.is_empty() {
            return Err(Error::Invalid(
                "artifact chunk list must not be empty".into(),
            ));
        }
        let mut next_offset = 0_u64;
        for (index, chunk) in self.chunks.iter().enumerate() {
            validate_digest(&chunk.sha256, "chunk digest")?;
            if chunk.offset != next_offset || chunk.size_bytes == 0 {
                return Err(Error::Invalid(
                    "artifact chunks must be contiguous and non-empty".into(),
                ));
            }
            let is_last = index + 1 == self.chunks.len();
            if !is_last && chunk.size_bytes != self.chunk_size_bytes {
                return Err(Error::Invalid("non-final chunk has the wrong size".into()));
            }
            if chunk.size_bytes > self.chunk_size_bytes {
                return Err(Error::Invalid(
                    "chunk exceeds the declared chunk size".into(),
                ));
            }
            next_offset = next_offset
                .checked_add(chunk.size_bytes)
                .ok_or_else(|| Error::Invalid("chunk coverage overflows u64".into()))?;
        }
        if next_offset != self.artifact_size_bytes {
            return Err(Error::Invalid(
                "chunk coverage does not match artifact size".into(),
            ));
        }
        Ok(())
    }

    fn validate_caches(&self) -> Result<(), Error> {
        let mut previous = None;
        for cache in &self.cache_generations {
            validate_label(&cache.name, "cache name")?;
            validate_label(&cache.generation, "cache generation")?;
            validate_digest(&cache.sha256, "cache generation digest")?;
            if previous.is_some_and(|value: &str| value >= cache.name.as_str()) {
                return Err(Error::Invalid(
                    "cache generations must be sorted and unique".into(),
                ));
            }
            previous = Some(&cache.name);
        }
        Ok(())
    }

    /// Require every mandatory cache to be present at the exact immutable generation.
    pub fn validate_cache_inventory(&self, available: &[CacheGeneration]) -> Result<(), Error> {
        self.validate()?;
        for required in self.cache_generations.iter().filter(|cache| cache.required) {
            if !available.iter().any(|cache| {
                cache.name == required.name
                    && cache.generation == required.generation
                    && cache.sha256 == required.sha256
            }) {
                return Err(Error::Invalid(format!(
                    "required cache {} is absent or has a different generation",
                    required.name
                )));
            }
        }
        Ok(())
    }

    /// SHA-256 of the canonical JSON representation after validation.
    pub fn canonical_sha256(&self) -> Result<String, Error> {
        self.validate()?;
        Ok(sha256_bytes(&serde_json::to_vec(self)?))
    }

    /// Require queue authority to bind this exact manifest and producer fence.
    pub fn validate_authority(&self, authority: &ManifestAuthority) -> Result<(), Error> {
        self.validate()?;
        validate_digest(&authority.manifest_sha256, "authority manifest digest")?;
        let actual_manifest = self.canonical_sha256()?;
        let exact = authority.manifest_sha256 == actual_manifest
            && authority.repository == self.source.repository
            && authority.head_sha == self.source.head_sha
            && authority.tree_sha == self.source.tree_sha
            && authority.worker_id == self.producer.worker_id
            && authority.lease_id == self.producer.lease_id
            && authority.generation == self.producer.generation
            && authority.attempt == self.producer.attempt;
        if !exact {
            return Err(Error::StaleFence(
                "manifest no longer matches the authorized source and producer fence".into(),
            ));
        }
        Ok(())
    }
}

/// Outcome of authenticating an interrupted partial artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumePlan {
    Restart,
    Append {
        verified_bytes: u64,
        truncate_to: u64,
    },
    CompletePendingFinalVerification,
}

/// Authenticate full chunks of a partial file and find the only safe append boundary.
pub fn plan_verified_resume(path: &Path, manifest: &ArtifactManifest) -> Result<ResumePlan, Error> {
    manifest.validate()?;
    reject_non_regular_file(path, "artifact partial")?;
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length > manifest.artifact_size_bytes {
        return Ok(ResumePlan::Restart);
    }
    let mut verified = 0_u64;
    for chunk in &manifest.chunks {
        if length < chunk.offset + chunk.size_bytes {
            break;
        }
        file.seek(SeekFrom::Start(chunk.offset))?;
        let mut take = (&mut file).take(chunk.size_bytes);
        let digest = sha256_reader(&mut take)?;
        if digest != chunk.sha256 {
            return if verified == 0 {
                Ok(ResumePlan::Restart)
            } else {
                Ok(ResumePlan::Append {
                    verified_bytes: verified,
                    truncate_to: verified,
                })
            };
        }
        verified += chunk.size_bytes;
    }
    if verified == manifest.artifact_size_bytes && length == verified {
        Ok(ResumePlan::CompletePendingFinalVerification)
    } else if verified == 0 {
        Ok(ResumePlan::Restart)
    } else {
        Ok(ResumePlan::Append {
            verified_bytes: verified,
            truncate_to: verified,
        })
    }
}

/// Truncate a partial file to the authenticated boundary selected by [`plan_verified_resume`].
pub fn apply_resume_plan(path: &Path, plan: ResumePlan) -> Result<(), Error> {
    let length = match plan {
        ResumePlan::Restart => 0,
        ResumePlan::Append { truncate_to, .. } => truncate_to,
        ResumePlan::CompletePendingFinalVerification => return Ok(()),
    };
    reject_non_regular_file(path, "artifact partial")?;
    OpenOptions::new().write(true).open(path)?.set_len(length)?;
    Ok(())
}

/// Shell-free command description for receiver-pull `rsync`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiverPullCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// Fully validated inputs for one receiver-pull invocation.
pub struct ReceiverPullRequest<'a> {
    pub rsync_program: &'a Path,
    pub remote_host: &'a str,
    pub remote_store_root: &'a Path,
    pub local_store: &'a ArtifactStore,
    pub transfer_session: &'a str,
    pub artifact_sha256: &'a str,
    pub resume: ResumePlan,
    pub timeout_seconds: u32,
}

/// Build receiver-side `rsync` argv from trusted roots and a validated digest.
pub fn receiver_pull_command(
    request: &ReceiverPullRequest<'_>,
) -> Result<ReceiverPullCommand, Error> {
    let ReceiverPullRequest {
        rsync_program,
        remote_host,
        remote_store_root,
        local_store,
        transfer_session,
        artifact_sha256,
        resume,
        timeout_seconds,
    } = request;
    if !rsync_program.is_absolute() || rsync_program.file_name().is_none() {
        return Err(Error::Invalid(
            "rsync program must be an absolute path".into(),
        ));
    }
    validate_host(remote_host)?;
    validate_digest(artifact_sha256, "artifact digest")?;
    let root = portable_absolute_path(remote_store_root)?;
    let local_partial = local_store.partial_path(artifact_sha256, transfer_session)?;
    if local_partial.exists() {
        reject_non_regular_file(&local_partial, "artifact partial")?;
    }
    if *timeout_seconds == 0 {
        return Err(Error::Invalid("rsync timeout must be non-zero".into()));
    }
    let remote = format!("{remote_host}:{root}/objects/{artifact_sha256}.tar.zst");
    let mut args = vec![OsString::from("-a"), OsString::from("--partial")];
    match *resume {
        ResumePlan::Append {
            verified_bytes,
            truncate_to,
        } if verified_bytes > 0 && verified_bytes == truncate_to => {
            args.push(OsString::from("--append"));
        }
        ResumePlan::Append { .. } => {
            return Err(Error::Invalid(
                "append requires a non-zero verified boundary".into(),
            ));
        }
        ResumePlan::Restart => {}
        ResumePlan::CompletePendingFinalVerification => {
            return Err(Error::Invalid(
                "completed partial must be published, not transferred again".into(),
            ));
        }
    }
    args.extend([
        OsString::from("--stats"),
        OsString::from(format!("--timeout={timeout_seconds}")),
        OsString::from(format!("--contimeout={timeout_seconds}")),
        OsString::from("--"),
        OsString::from(remote),
        local_partial.into_os_string(),
    ]);
    Ok(ReceiverPullCommand {
        program: rsync_program.to_path_buf(),
        args,
    })
}

/// Disk reserve enforced before receiving more artifact bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpacePolicy {
    pub minimum_free_bytes: u64,
}

impl SpacePolicy {
    /// Check a supplied free-space observation without filesystem I/O.
    pub fn check(self, available: u64, artifact_size: u64, partial_size: u64) -> Result<(), Error> {
        if partial_size > artifact_size {
            return Err(Error::Invalid(
                "partial size exceeds the declared artifact size".into(),
            ));
        }
        let remaining = artifact_size - partial_size;
        let required = remaining
            .checked_add(self.minimum_free_bytes)
            .ok_or_else(|| Error::Invalid("space requirement overflows u64".into()))?;
        if available < required {
            return Err(Error::InsufficientSpace {
                required,
                available,
            });
        }
        Ok(())
    }

    /// Check current free space for a host-declared staging directory.
    pub fn check_path(
        self,
        staging_directory: &Path,
        artifact_size: u64,
        partial_size: u64,
    ) -> Result<(), Error> {
        self.check(
            available_space(staging_directory)?,
            artifact_size,
            partial_size,
        )
    }
}

/// Digest-addressed artifact staging and publication root.
#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
}

/// Verified publication result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    Published(PathBuf),
    Reused(PathBuf),
}

impl ArtifactStore {
    /// Open or create a caller-authorized store with same-root incoming/object directories.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(Error::Invalid(
                "artifact store root must be absolute".into(),
            ));
        }
        // Reject a symlink at the authority boundary, but canonicalize trusted
        // ancestors (macOS commonly exposes `/var` through `/private/var`).
        reject_symlink(&root)?;
        let root = if root.exists() {
            root.canonicalize()?
        } else {
            let parent = root
                .parent()
                .ok_or_else(|| Error::Invalid("artifact store root has no parent".into()))?
                .canonicalize()?;
            let name = root
                .file_name()
                .ok_or_else(|| Error::Invalid("artifact store root has no name".into()))?;
            parent.join(name)
        };
        ensure_private_directory(&root)?;
        for child in [root.join(".incoming"), root.join("objects")] {
            reject_symlink(&child)?;
            ensure_private_directory(&child)?;
            if !child.metadata()?.is_dir() {
                return Err(Error::Invalid(
                    "artifact store component is not a directory".into(),
                ));
            }
        }
        Ok(Self { root })
    }

    /// Derive the only accepted partial path for a digest and transfer session.
    pub fn partial_path(&self, digest: &str, session: &str) -> Result<PathBuf, Error> {
        validate_digest(digest, "artifact digest")?;
        validate_label(session, "transfer session")?;
        Ok(self
            .root
            .join(".incoming")
            .join(format!("{digest}.{session}.partial")))
    }

    /// Verify exact manifest authority, bytes, and chunks, then atomically publish.
    pub fn publish_verified(
        &self,
        manifest: &ArtifactManifest,
        authority: &ManifestAuthority,
        session: &str,
    ) -> Result<PublishOutcome, Error> {
        manifest.validate_authority(authority)?;
        let partial = self.partial_path(&manifest.artifact_sha256, session)?;
        let metadata = fs::symlink_metadata(&partial)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(Error::Invalid(
                "artifact partial must be a regular non-symlink file".into(),
            ));
        }
        if metadata.len() != manifest.artifact_size_bytes {
            return Err(Error::Invalid("artifact partial has the wrong size".into()));
        }
        if plan_verified_resume(&partial, manifest)? != ResumePlan::CompletePendingFinalVerification
        {
            return Err(Error::Invalid(
                "artifact partial failed chunk verification".into(),
            ));
        }
        let mut file = File::open(&partial)?;
        if sha256_reader(&mut file)? != manifest.artifact_sha256 {
            return Err(Error::Invalid("artifact final digest mismatch".into()));
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&partial)?
            .sync_all()?;
        let destination = self
            .root
            .join("objects")
            .join(format!("{}.tar.zst", manifest.artifact_sha256));
        if destination.exists() {
            let existing = fs::symlink_metadata(&destination)?;
            if !existing.file_type().is_file()
                || existing.file_type().is_symlink()
                || existing.len() != manifest.artifact_size_bytes
            {
                return Err(Error::Invalid(
                    "published artifact path is not the expected immutable object".into(),
                ));
            }
            let mut existing_file = File::open(&destination)?;
            if sha256_reader(&mut existing_file)? != manifest.artifact_sha256 {
                return Err(Error::Invalid(
                    "published artifact digest conflicts with manifest".into(),
                ));
            }
            fs::remove_file(&partial)?;
            return Ok(PublishOutcome::Reused(destination));
        }
        fs::rename(&partial, &destination)?;
        sync_directory(destination.parent().expect("destination has parent"))?;
        Ok(PublishOutcome::Published(destination))
    }
}

fn sha256_reader(reader: &mut impl Read) -> Result<String, Error> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn validate_digest(value: &str, label: &str) -> Result<(), Error> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Error::Invalid(format!("{label} must be lowercase SHA-256")));
    }
    Ok(())
}

fn validate_git_oid(value: &str, label: &str) -> Result<(), Error> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Error::Invalid(format!(
            "{label} must be a lowercase Git object id"
        )));
    }
    Ok(())
}

fn validate_repository(value: &str) -> Result<(), Error> {
    let mut pieces = value.split('/');
    if pieces.clone().count() != 2 || pieces.any(|piece| validate_simple(piece).is_err()) {
        return Err(Error::Invalid(
            "repository must be safe owner/name form".into(),
        ));
    }
    Ok(())
}

fn validate_label(value: &str, label: &str) -> Result<(), Error> {
    validate_simple(value)
        .map_err(|()| Error::Invalid(format!("{label} contains unsafe characters")))
}

fn validate_simple(value: &str) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > 128
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Err(())
    } else {
        Ok(())
    }
}

fn validate_host(value: &str) -> Result<(), Error> {
    validate_label(value, "remote host")
}

fn validate_mode(mode: u32) -> Result<(), Error> {
    if mode & !0o777 != 0 {
        return Err(Error::Invalid(
            "layout mode may contain only portable permission bits".into(),
        ));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), Error> {
    if value.is_empty()
        || value.contains('\\')
        || value.contains('\0')
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'/' | b'.' | b'_' | b'-'))
        || value
            .split('/')
            .any(|piece| piece.is_empty() || matches!(piece, "." | ".."))
    {
        return Err(Error::Invalid(
            "layout path is empty or non-portable".into(),
        ));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::Invalid(
            "layout path must be normalized and relative".into(),
        ));
    }
    Ok(())
}

fn portable_absolute_path(path: &Path) -> Result<String, Error> {
    let value = path
        .to_str()
        .ok_or_else(|| Error::Invalid("remote store root must be UTF-8".into()))?;
    if !path.is_absolute()
        || value.contains('\\')
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'/' | b'.' | b'_' | b'-'))
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(Error::Invalid(
            "remote store root must be a portable absolute path".into(),
        ));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn reject_symlink(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Invalid(
            "artifact store path must not be a symlink".into(),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reject_non_regular_file(path: &Path, label: &str) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid(format!(
            "{label} must be a regular non-symlink file"
        )));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), Error> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), Error> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const TEST_CHUNK_SIZE: usize = 64 * 1024;

    fn digest(bytes: &[u8]) -> String {
        sha256_bytes(bytes)
    }

    fn fixture(bytes: &[u8]) -> ArtifactManifest {
        let entries = vec![LayoutEntry::File {
            path: "bin/tool".into(),
            mode: 0o755,
            size_bytes: 4,
            sha256: digest(b"tool"),
        }];
        let chunks = bytes
            .chunks(TEST_CHUNK_SIZE)
            .enumerate()
            .map(|(index, chunk)| ArtifactChunk {
                offset: index as u64 * MIN_CHUNK_SIZE,
                size_bytes: chunk.len() as u64,
                sha256: digest(chunk),
            })
            .collect();
        ArtifactManifest {
            schema: MANIFEST_SCHEMA,
            source: SourceIdentity {
                repository: "Generous-Corp/pulp".into(),
                head_sha: "a".repeat(40),
                tree_sha: "b".repeat(40),
            },
            build: BuildIdentity {
                platform: "macos".into(),
                architecture: "arm64".into(),
                build_type: "release".into(),
                toolchain_sha256: "c".repeat(64),
                golden_image_sha256: Some("d".repeat(64)),
                test_inventory_sha256: "e".repeat(64),
            },
            format: ArtifactFormat::TarZstd,
            artifact_sha256: digest(bytes),
            artifact_size_bytes: bytes.len() as u64,
            chunk_size_bytes: MIN_CHUNK_SIZE,
            layout_sha256: digest(&serde_json::to_vec(&entries).unwrap()),
            entries,
            chunks,
            cache_generations: vec![CacheGeneration {
                name: "skia".into(),
                generation: "m138-v1".into(),
                sha256: "f".repeat(64),
                required: true,
            }],
            producer: ProducerFence {
                worker_id: "m3-01".into(),
                lease_id: "lease-7".into(),
                generation: 9,
                attempt: 2,
            },
        }
    }

    fn authority(manifest: &ArtifactManifest) -> ManifestAuthority {
        ManifestAuthority {
            manifest_sha256: manifest.canonical_sha256().unwrap(),
            repository: manifest.source.repository.clone(),
            head_sha: manifest.source.head_sha.clone(),
            tree_sha: manifest.source.tree_sha.clone(),
            worker_id: manifest.producer.worker_id.clone(),
            lease_id: manifest.producer.lease_id.clone(),
            generation: manifest.producer.generation,
            attempt: manifest.producer.attempt,
        }
    }

    #[test]
    fn validates_exact_identity_and_rejects_stale_fence() {
        let bytes = vec![7; TEST_CHUNK_SIZE + 31];
        let manifest = fixture(&bytes);
        manifest.validate_authority(&authority(&manifest)).unwrap();
        let mut stale = authority(&manifest);
        stale.generation += 1;
        assert!(matches!(
            manifest.validate_authority(&stale),
            Err(Error::StaleFence(_))
        ));
    }

    #[test]
    fn rejects_hostile_layout_and_chunk_gap() {
        let bytes = vec![1; TEST_CHUNK_SIZE + 2];
        for hostile in [
            "../escape",
            "/absolute",
            "dir\\file",
            "a/../b",
            "C:/windows",
            "a//b",
            "space bad",
        ] {
            let mut manifest = fixture(&bytes);
            if let LayoutEntry::File { path, .. } = &mut manifest.entries[0] {
                *path = hostile.into();
            }
            manifest.layout_sha256 = digest(&serde_json::to_vec(&manifest.entries).unwrap());
            assert!(manifest.validate().is_err(), "accepted {hostile}");
        }
        let mut manifest = fixture(&bytes);
        manifest.chunks[1].offset += 1;
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn rejects_unsorted_caches_and_layout_digest_corruption() {
        let bytes = vec![2; TEST_CHUNK_SIZE];
        let mut manifest = fixture(&bytes);
        manifest.cache_generations.push(CacheGeneration {
            name: "aaa".into(),
            generation: "v1".into(),
            sha256: "a".repeat(64),
            required: false,
        });
        assert!(manifest.validate().is_err());
        let mut manifest = fixture(&bytes);
        manifest.layout_sha256 = "0".repeat(64);
        assert!(manifest.validate().is_err());
        let mut manifest = fixture(&bytes);
        if let LayoutEntry::File { mode, .. } = &mut manifest.entries[0] {
            *mode = 0o4755;
        }
        manifest.layout_sha256 = digest(&serde_json::to_vec(&manifest.entries).unwrap());
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn required_cache_generation_must_match_exactly() {
        let manifest = fixture(&vec![2; TEST_CHUNK_SIZE]);
        manifest
            .validate_cache_inventory(&manifest.cache_generations)
            .unwrap();
        let mut wrong = manifest.cache_generations.clone();
        wrong[0].generation = "m139-v1".into();
        assert!(manifest.validate_cache_inventory(&wrong).is_err());
        assert!(manifest.validate_cache_inventory(&[]).is_err());
    }

    #[test]
    fn resume_authenticates_prefix_and_trims_tail_or_corruption() {
        let bytes = vec![3; TEST_CHUNK_SIZE * 2 + 10];
        let manifest = fixture(&bytes);
        let temp = TempDir::new().unwrap();
        let partial = temp.path().join("partial");
        fs::write(&partial, &bytes[..TEST_CHUNK_SIZE + 17]).unwrap();
        let plan = plan_verified_resume(&partial, &manifest).unwrap();
        assert_eq!(
            plan,
            ResumePlan::Append {
                verified_bytes: MIN_CHUNK_SIZE,
                truncate_to: MIN_CHUNK_SIZE
            }
        );
        apply_resume_plan(&partial, plan).unwrap();
        assert_eq!(fs::metadata(&partial).unwrap().len(), MIN_CHUNK_SIZE);

        let mut corrupt = bytes.clone();
        corrupt[TEST_CHUNK_SIZE + 1] ^= 1;
        fs::write(&partial, &corrupt).unwrap();
        assert_eq!(
            plan_verified_resume(&partial, &manifest).unwrap(),
            ResumePlan::Append {
                verified_bytes: MIN_CHUNK_SIZE,
                truncate_to: MIN_CHUNK_SIZE
            }
        );
    }

    #[test]
    fn resume_restarts_for_first_chunk_corruption_or_oversize() {
        let bytes = vec![4; TEST_CHUNK_SIZE];
        let manifest = fixture(&bytes);
        let temp = TempDir::new().unwrap();
        let partial = temp.path().join("partial");
        let mut corrupt = bytes.clone();
        corrupt[0] ^= 1;
        fs::write(&partial, &corrupt).unwrap();
        assert_eq!(
            plan_verified_resume(&partial, &manifest).unwrap(),
            ResumePlan::Restart
        );
        fs::write(&partial, vec![4; TEST_CHUNK_SIZE + 1]).unwrap();
        assert_eq!(
            plan_verified_resume(&partial, &manifest).unwrap(),
            ResumePlan::Restart
        );
    }

    #[test]
    fn receiver_pull_argv_is_shell_free_and_append_is_fenced() {
        let digest = "a".repeat(64);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let request = |program, host, root, session, resume| ReceiverPullRequest {
            rsync_program: Path::new(program),
            remote_host: host,
            remote_store_root: Path::new(root),
            local_store: &store,
            transfer_session: session,
            artifact_sha256: &digest,
            resume,
            timeout_seconds: 30,
        };
        let command = receiver_pull_command(&request(
            "/usr/bin/rsync",
            "m1-lan",
            "/var/lib/shipyard/artifacts",
            "run-1",
            ResumePlan::Append {
                verified_bytes: 65536,
                truncate_to: 65536,
            },
        ))
        .unwrap();
        assert_eq!(command.program, Path::new("/usr/bin/rsync"));
        assert!(command.args.iter().any(|arg| arg == "--append"));
        for hostile in ["-oProxyCommand=bad", "host;bad", "host name"] {
            assert!(
                receiver_pull_command(&request(
                    "/usr/bin/rsync",
                    hostile,
                    "/safe",
                    "run-1",
                    ResumePlan::Restart,
                ))
                .is_err()
            );
        }
        assert!(
            receiver_pull_command(&request(
                "rsync",
                "m1",
                "/safe",
                "run-1",
                ResumePlan::Restart,
            ))
            .is_err()
        );
        assert!(
            receiver_pull_command(&request(
                "/usr/bin/rsync",
                "m1",
                "/safe;bad",
                "run-1",
                ResumePlan::Restart,
            ))
            .is_err()
        );
        assert!(
            receiver_pull_command(&request(
                "/usr/bin/rsync",
                "m1",
                "/safe",
                "run-1",
                ResumePlan::Append {
                    verified_bytes: 0,
                    truncate_to: 0
                },
            ))
            .is_err()
        );
        assert!(
            receiver_pull_command(&request(
                "/usr/bin/rsync",
                "m1",
                "/safe",
                "../escape",
                ResumePlan::Restart,
            ))
            .is_err()
        );
    }

    #[test]
    fn space_policy_includes_remaining_bytes_and_watermark() {
        let policy = SpacePolicy {
            minimum_free_bytes: 100,
        };
        policy.check(150, 100, 50).unwrap();
        assert!(matches!(
            policy.check(149, 100, 50),
            Err(Error::InsufficientSpace { .. })
        ));
        assert!(policy.check(u64::MAX, u64::MAX, 0).is_err());
        assert!(policy.check(1_000, 100, 101).is_err());
    }

    #[test]
    fn publication_is_atomic_reusable_and_preserves_corruption() {
        let bytes = vec![5; TEST_CHUNK_SIZE + 9];
        let manifest = fixture(&bytes);
        let auth = authority(&manifest);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let partial = store
            .partial_path(&manifest.artifact_sha256, "run-1")
            .unwrap();
        fs::write(&partial, &bytes).unwrap();
        let outcome = store.publish_verified(&manifest, &auth, "run-1").unwrap();
        let PublishOutcome::Published(published) = outcome else {
            panic!("expected publication")
        };
        assert!(!partial.exists());
        assert_eq!(fs::read(&published).unwrap(), bytes);

        let second = store
            .partial_path(&manifest.artifact_sha256, "run-2")
            .unwrap();
        fs::write(&second, &bytes).unwrap();
        assert!(matches!(
            store.publish_verified(&manifest, &auth, "run-2").unwrap(),
            PublishOutcome::Reused(_)
        ));

        let bad = store
            .partial_path(&manifest.artifact_sha256, "run-3")
            .unwrap();
        let mut corrupt = bytes.clone();
        corrupt[0] ^= 1;
        fs::write(&bad, corrupt).unwrap();
        assert!(store.publish_verified(&manifest, &auth, "run-3").is_err());
        assert!(bad.exists());
    }

    #[test]
    fn unknown_manifest_fields_are_rejected() {
        let manifest = fixture(&vec![8; TEST_CHUNK_SIZE]);
        let mut value = serde_json::to_value(manifest).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".into(), serde_json::json!(true));
        assert!(serde_json::from_value::<ArtifactManifest>(value).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn store_rejects_symlink_components_and_partial() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = temp.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(ArtifactStore::open(&link).is_err());

        let bytes = vec![9; TEST_CHUNK_SIZE];
        let manifest = fixture(&bytes);
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let external = temp.path().join("external");
        fs::write(&external, &bytes).unwrap();
        symlink(
            &external,
            store
                .partial_path(&manifest.artifact_sha256, "run")
                .unwrap(),
        )
        .unwrap();
        assert!(
            store
                .publish_verified(&manifest, &authority(&manifest), "run")
                .is_err()
        );
    }
}
