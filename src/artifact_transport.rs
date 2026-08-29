//! Pure, default-off primitives for transferring immutable build artifacts.
//!
//! This module deliberately does not dispatch work or select hosts. A caller must
//! provide an exact manifest authority, an already-authorized staging root, and
//! a receiver-pull transport. Publication is fail-closed and digest addressed.
#![allow(missing_docs)]

use fs2::{FileExt, available_space};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

/// Current on-disk/wire manifest schema.
pub const MANIFEST_SCHEMA: u32 = 3;
const PREVIOUS_MANIFEST_SCHEMA: u32 = 2;
const LEGACY_MANIFEST_SCHEMA: u32 = 1;
/// Default chunk size for newly-created manifests.
pub const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
const MIN_CHUNK_SIZE: u64 = 64 * 1024;
const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024;
const MAX_LAYOUT_ENTRIES: usize = 100_000;
const MAX_LAYOUT_PATH_BYTES: usize = 1024;
const MAX_LAYOUT_PATH_DEPTH: usize = 64;
const MAX_LAYOUT_COMPONENT_BYTES: usize = 255;
const MAX_LAYOUT_PREFIXES: usize = 250_000;
// GNU tar's default blocking factor is twenty 512-byte records. Supporting
// that canonical padding preserves common-producer interoperability without
// allowing an authenticated tiny archive to expand into unbounded zero data.
const MAX_TAR_ZERO_PADDING_BYTES: usize = 20 * 512;
const ENTRY_ALLOCATION_RESERVE_BYTES: u64 = 64 * 1024;
const MAX_ZSTD_WINDOW_LOG: u32 = 25;

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

/// Durability state after an archive has been atomically published.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "callers must distinguish durable publication from a pending parent sync"]
pub enum PublicationOutcome {
    /// The destination and its parent-directory entry are durable.
    Durable,
    /// The destination is visible and complete, but syncing its parent failed.
    PublishedParentSyncPending {
        destination: PathBuf,
        message: String,
    },
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_mode: Option<u32>,
    pub entries: Vec<LayoutEntry>,
    pub chunks: Vec<ArtifactChunk>,
    pub cache_generations: Vec<CacheGeneration>,
    pub producer: ProducerFence,
}

/// Controller-owned identities required to package one complete build tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildTreeArtifactInputs {
    pub source: SourceIdentity,
    pub build: BuildIdentity,
    pub cache_generations: Vec<CacheGeneration>,
    pub producer: ProducerFence,
}

/// Durable result of packaging a complete build tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildTreePackOutcome {
    pub manifest: ArtifactManifest,
    pub publication: PublicationOutcome,
}

/// Result of replacing a mutable build tree with a verified immutable tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildTreeRestoreOutcome {
    pub publication: PublicationOutcome,
    pub replaced_existing: bool,
    pub quarantine_cleanup_pending: Option<PathBuf>,
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
        if !matches!(
            self.schema,
            LEGACY_MANIFEST_SCHEMA | PREVIOUS_MANIFEST_SCHEMA | MANIFEST_SCHEMA
        ) {
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
        match (self.schema, self.root_mode) {
            (MANIFEST_SCHEMA, Some(mode)) => validate_mode(mode)?,
            (MANIFEST_SCHEMA, None) => {
                return Err(Error::Invalid(
                    "current manifest schema requires the build-tree root mode".into(),
                ));
            }
            (_, Some(_)) => {
                return Err(Error::Invalid(
                    "older manifest schemas must not declare a build-tree root mode".into(),
                ));
            }
            (_, None) => {}
        }
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
        if self.entries.len() > MAX_LAYOUT_ENTRIES {
            return Err(Error::Invalid(
                "artifact layout exceeds the supported entry count".into(),
            ));
        }
        let mut previous = None;
        let mut portable_paths: HashMap<(usize, String), (usize, String)> =
            HashMap::with_capacity(self.entries.len());
        let mut next_prefix_id = 1_usize;
        for entry in &self.entries {
            validate_relative_path(entry.path())?;
            let mut parent_id = 0_usize;
            for component in entry.path().split('/') {
                validate_portable_component(component)?;
                let key = (parent_id, component.to_ascii_lowercase());
                match portable_paths.entry(key) {
                    Entry::Occupied(existing) => {
                        let (prefix_id, spelling) = existing.get();
                        if spelling != component {
                            return Err(Error::Invalid(
                                "layout paths must remain unique on case-insensitive filesystems"
                                    .into(),
                            ));
                        }
                        parent_id = *prefix_id;
                    }
                    Entry::Vacant(vacant) => {
                        if next_prefix_id > MAX_LAYOUT_PREFIXES {
                            return Err(Error::Invalid(
                                "artifact layout exceeds the supported path-prefix count".into(),
                            ));
                        }
                        vacant.insert((next_prefix_id, component.to_owned()));
                        parent_id = next_prefix_id;
                        next_prefix_id += 1;
                    }
                }
            }
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
        if self.schema >= PREVIOUS_MANIFEST_SCHEMA {
            let by_path: HashMap<&str, &LayoutEntry> = self
                .entries
                .iter()
                .map(|entry| (entry.path(), entry))
                .collect();
            for entry in &self.entries {
                let mut parent = Path::new(entry.path()).parent();
                while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
                    let parent_path = path.to_str().ok_or_else(|| {
                        Error::Invalid("layout parent path must be portable UTF-8".into())
                    })?;
                    if !matches!(
                        by_path.get(parent_path),
                        Some(LayoutEntry::Directory { .. })
                    ) {
                        return Err(Error::Invalid(format!(
                            "layout omits parent directory {parent_path}"
                        )));
                    }
                    parent = path.parent();
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

    fn unpacked_allocation_budget_bytes(&self) -> Result<u64, Error> {
        let mut directories: HashSet<&str> = HashSet::new();
        for entry in &self.entries {
            let mut parent = match entry {
                LayoutEntry::Directory { path, .. } => Some(path.as_str()),
                LayoutEntry::File { path, .. } => path.rsplit_once('/').map(|(parent, _)| parent),
            };
            while let Some(path) = parent.filter(|path| !path.is_empty()) {
                directories.insert(path);
                parent = path.rsplit_once('/').map(|(parent, _)| parent);
            }
        }
        let metadata = u64::try_from(directories.len())
            .map_err(|_| Error::Invalid("layout entry count overflows u64".into()))?
            .checked_add(
                u64::try_from(
                    self.entries
                        .iter()
                        .filter(|entry| matches!(entry, LayoutEntry::File { .. }))
                        .count(),
                )
                .map_err(|_| Error::Invalid("layout entry count overflows u64".into()))?,
            )
            .and_then(|count| count.checked_mul(ENTRY_ALLOCATION_RESERVE_BYTES))
            .ok_or_else(|| Error::Invalid("unpacked allocation budget overflows u64".into()))?;
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                LayoutEntry::File { size_bytes, .. } => Some(*size_bytes),
                LayoutEntry::Directory { .. } => None,
            })
            .try_fold(metadata, |total, size| {
                total.checked_add(size).ok_or_else(|| {
                    Error::Invalid("unpacked allocation budget overflows u64".into())
                })
            })
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

/// Package every regular file and directory in `source_root` into one
/// deterministic, content-addressed tar+zstd artifact.
///
/// The source root and archive destination must be absolute. Links, special
/// files, nonportable paths, concurrent tree mutation, and an existing archive
/// destination fail closed. The returned manifest has already passed the same
/// archive/layout verifier used by restoration.
pub fn pack_verified_build_tree(
    source_root: &Path,
    archive_destination: &Path,
    inputs: BuildTreeArtifactInputs,
) -> Result<BuildTreePackOutcome, Error> {
    validate_build_tree_root(source_root)?;
    validate_new_archive_destination(archive_destination)?;
    let observed = observe_build_tree(source_root)?;
    let entries = observed.entries.clone();
    let parent = archive_destination
        .parent()
        .ok_or_else(|| Error::Invalid("artifact destination has no parent".into()))?;
    let staging = tempfile::Builder::new()
        .prefix(".shipyard-build-tree-")
        .tempfile_in(parent)?;
    write_build_tree_archive(&observed, staging.as_file())?;
    staging.as_file().sync_all()?;
    validate_observed_build_tree(source_root, &observed, &entries)?;
    let (artifact_sha256, artifact_size_bytes, chunks) =
        digest_archive_chunks(staging.path(), DEFAULT_CHUNK_SIZE)?;
    let manifest = ArtifactManifest {
        schema: MANIFEST_SCHEMA,
        source: inputs.source,
        build: inputs.build,
        format: ArtifactFormat::TarZstd,
        artifact_sha256,
        artifact_size_bytes,
        chunk_size_bytes: DEFAULT_CHUNK_SIZE,
        layout_sha256: sha256_bytes(&serde_json::to_vec(&entries)?),
        root_mode: Some(observed.root_mode),
        entries,
        chunks,
        cache_generations: inputs.cache_generations,
        producer: inputs.producer,
    };
    manifest.validate()?;
    verify_archive_layout(staging.path(), &manifest)?;
    let staging_path = staging
        .into_temp_path()
        .keep()
        .map_err(|error| error.error)?;
    if let Err(error) = rename_no_replace(&staging_path, archive_destination) {
        let _ = fs::remove_file(&staging_path);
        return Err(error);
    }
    let publication = publication_outcome(archive_destination, parent, sync_directory);
    Ok(BuildTreePackOutcome {
        manifest,
        publication,
    })
}

/// Replace an existing mutable build tree with the exact verified archive.
///
/// The current tree is quarantined under the destination parent before the
/// verified tree is published. Any extraction failure restores the prior tree
/// before returning. A successfully displaced old tree remains quarantined and
/// is returned to the caller; only a caller holding the production mutation
/// fence may remove it after revalidating the installed destination.
pub fn restore_verified_build_tree(
    archive: &Path,
    manifest: &ArtifactManifest,
    authority: &ManifestAuthority,
    destination: &Path,
    space_policy: SpacePolicy,
) -> Result<BuildTreeRestoreOutcome, Error> {
    restore_verified_build_tree_with(
        archive,
        manifest,
        authority,
        destination,
        space_policy,
        extract_verified_archive_locked,
    )
}

fn restore_verified_build_tree_with(
    archive: &Path,
    manifest: &ArtifactManifest,
    authority: &ManifestAuthority,
    destination: &Path,
    space_policy: SpacePolicy,
    extract: impl FnOnce(
        &Path,
        &ArtifactManifest,
        &ManifestAuthority,
        &Path,
        SpacePolicy,
    ) -> Result<PublicationOutcome, Error>,
) -> Result<BuildTreeRestoreOutcome, Error> {
    manifest.validate_authority(authority)?;
    verify_archive_layout(archive, manifest)?;
    let parent = validate_restore_destination(destination)?;
    let _lock = acquire_extraction_lock(parent)?;
    manifest.validate_authority(authority)?;
    verify_archive_layout(archive, manifest)?;

    let mut quarantine = None;
    if destination.exists() {
        let metadata = fs::symlink_metadata(destination)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::Invalid(
                "existing build-tree destination must be a real directory".into(),
            ));
        }
        let directory = tempfile::Builder::new()
            .prefix(".shipyard-build-tree-quarantine-")
            .tempdir_in(parent)?;
        // Persist the quarantine before moving user data into it. A TempDir
        // guard would recursively delete the prior build on any later error.
        let directory = directory.keep();
        if let Err(error) = sync_directory(parent) {
            let _ = fs::remove_dir(&directory);
            let _ = sync_directory(parent);
            return Err(error);
        }
        let prior = directory.join("prior");
        if let Err(error) = fs::rename(destination, &prior) {
            let _ = fs::remove_dir(&directory);
            return Err(error.into());
        }
        if let Err(error) = sync_directory(&directory).and_then(|()| sync_directory(parent)) {
            rollback_quarantined_tree(destination, parent, &directory, &prior, &error)?;
            return Err(error);
        }
        quarantine = Some((directory, prior));
    }

    let publication = match extract(archive, manifest, authority, destination, space_policy) {
        Ok(publication) => publication,
        Err(extract_error) => {
            if let Some((directory, prior)) = quarantine.as_ref() {
                if destination.exists() {
                    return Err(Error::Invalid(format!(
                        "verified restore failed ({extract_error}); destination reappeared before rollback; prior tree preserved at {}",
                        prior.display()
                    )));
                }
                rollback_quarantined_tree(destination, parent, directory, prior, &extract_error)?;
            }
            return Err(extract_error);
        }
    };

    let replaced_existing = quarantine.is_some();
    // This transport primitive does not own the production mutation fence, so
    // it cannot prove that the published path still names this replacement at
    // cleanup time. Preserve the prior tree for fenced caller reconciliation.
    let quarantine_cleanup_pending = quarantine.map(|(directory, _)| directory);
    Ok(BuildTreeRestoreOutcome {
        publication,
        replaced_existing,
        quarantine_cleanup_pending,
    })
}

fn rollback_quarantined_tree(
    destination: &Path,
    parent: &Path,
    quarantine: &Path,
    prior: &Path,
    original_error: &Error,
) -> Result<(), Error> {
    rollback_quarantined_tree_with(
        destination,
        parent,
        quarantine,
        prior,
        original_error,
        rename_no_replace,
    )
}

fn rollback_quarantined_tree_with(
    destination: &Path,
    parent: &Path,
    quarantine: &Path,
    prior: &Path,
    original_error: &Error,
    restore: impl FnOnce(&Path, &Path) -> Result<(), Error>,
) -> Result<(), Error> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(Error::Invalid(format!(
                "verified restore failed ({original_error}); destination reappeared before rollback; prior tree preserved at {}",
                prior.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    restore(prior, destination).map_err(|rollback_error| {
        Error::Invalid(format!(
            "verified restore failed ({original_error}); rollback failed ({rollback_error}); prior tree preserved at {}",
            prior.display()
        ))
    })?;
    sync_directory(quarantine).map_err(|rollback_error| {
        Error::Invalid(format!(
            "verified restore failed ({original_error}); rollback quarantine sync failed ({rollback_error}); restored tree is visible at {} and quarantine remains at {}",
            destination.display(),
            quarantine.display()
        ))
    })?;
    sync_directory(parent).map_err(|rollback_error| {
        Error::Invalid(format!(
            "verified restore failed ({original_error}); rollback directory sync failed ({rollback_error}); restored tree is visible at {}",
            destination.display()
        ))
    })?;
    fs::remove_dir(quarantine)?;
    sync_directory(parent)?;
    Ok(())
}

fn validate_build_tree_root(root: &Path) -> Result<(), Error> {
    if !root.is_absolute() {
        return Err(Error::Invalid("build-tree root must be absolute".into()));
    }
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid(
            "build-tree root must be a real directory".into(),
        ));
    }
    Ok(())
}

/// Produce a stable, fully verified inventory of one immutable directory tree.
///
/// The tree is opened through no-follow directory handles, every regular file
/// is hashed, and the complete inventory is observed a second time before it is
/// returned. Links, special files, hard links, concurrent mutation, and
/// unsupported controller platforms fail closed. The operation is read-only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedImmutableTreeInventory {
    /// Portable mode of the pinned root directory.
    pub root_mode: u32,
    /// Complete sorted inventory below the root.
    pub entries: Vec<LayoutEntry>,
}

pub fn verified_immutable_tree_inventory(
    root: &Path,
) -> Result<VerifiedImmutableTreeInventory, Error> {
    validate_build_tree_root(root)?;
    let observed = observe_build_tree(root)?;
    let inventory = VerifiedImmutableTreeInventory {
        root_mode: observed.root_mode,
        entries: observed.entries.clone(),
    };
    validate_observed_build_tree(root, &observed, &inventory.entries)?;
    Ok(inventory)
}

fn validate_observed_build_tree(
    root_path: &Path,
    observed: &ObservedBuildTree,
    expected_entries: &[LayoutEntry],
) -> Result<(), Error> {
    if observe_pinned_build_tree(&observed.root)? != expected_entries
        || portable_mode(&observed.root.metadata()?) != observed.root_mode
        || same_file::Handle::from_file(open_pinned_root(root_path)?)? != observed.root_identity
    {
        return Err(Error::Invalid(
            "build tree changed while it was being packaged".into(),
        ));
    }
    Ok(())
}

fn validate_new_archive_destination(destination: &Path) -> Result<(), Error> {
    if !destination.is_absolute() {
        return Err(Error::Invalid(
            "build-tree archive destination must be absolute".into(),
        ));
    }
    reject_symlink(destination)?;
    if destination.exists() {
        return Err(Error::Invalid(
            "build-tree archive destination already exists".into(),
        ));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Invalid("build-tree archive destination has no parent".into()))?;
    reject_symlink(parent)?;
    if !parent.metadata()?.is_dir() {
        return Err(Error::Invalid(
            "build-tree archive parent is not a directory".into(),
        ));
    }
    Ok(())
}

fn validate_restore_destination(destination: &Path) -> Result<&Path, Error> {
    if !destination.is_absolute() {
        return Err(Error::Invalid(
            "build-tree restore destination must be absolute".into(),
        ));
    }
    reject_symlink(destination)?;
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Invalid("build-tree restore destination has no parent".into()))?;
    reject_symlink(parent)?;
    if !parent.metadata()?.is_dir() {
        return Err(Error::Invalid(
            "build-tree restore parent is not a directory".into(),
        ));
    }
    Ok(parent)
}

struct ObservedBuildTree {
    root: File,
    root_identity: same_file::Handle,
    root_mode: u32,
    entries: Vec<LayoutEntry>,
}

#[cfg(unix)]
fn observe_build_tree(root: &Path) -> Result<ObservedBuildTree, Error> {
    let root = open_pinned_root(root)?;
    let root_identity = same_file::Handle::from_file(root.try_clone()?)?;
    let root_mode = portable_mode(&root.metadata()?);
    let entries = observe_pinned_build_tree(&root)?;
    Ok(ObservedBuildTree {
        root,
        root_identity,
        root_mode,
        entries,
    })
}

#[cfg(unix)]
fn open_pinned_root(root: &Path) -> Result<File, Error> {
    use rustix::fs::{Mode, OFlags, open};

    Ok(File::from(
        open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| Error::Io(error.into()))?,
    ))
}

#[cfg(not(unix))]
fn open_pinned_root(_root: &Path) -> Result<File, Error> {
    Err(Error::Invalid(
        "verified build-tree packing requires no-follow directory handles on this platform".into(),
    ))
}

#[cfg(not(unix))]
fn observe_build_tree(_root: &Path) -> Result<ObservedBuildTree, Error> {
    Err(Error::Invalid(
        "verified build-tree packing requires no-follow directory handles on this platform".into(),
    ))
}

#[cfg(unix)]
fn observe_pinned_build_tree(root: &File) -> Result<Vec<LayoutEntry>, Error> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    fn visit(
        directory: &File,
        relative: &Path,
        entries: &mut Vec<LayoutEntry>,
    ) -> Result<(), Error> {
        let mut children = rustix::fs::Dir::read_from(directory)
            .map_err(|error| Error::Io(error.into()))?
            .map(|entry| {
                entry
                    .map(|entry| entry.file_name().to_bytes().to_vec())
                    .map_err(|error| Error::Io(error.into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        children.retain(|name| name.as_slice() != b"." && name.as_slice() != b"..");
        children.sort();
        for name in children {
            let name = OsStr::from_bytes(&name);
            let child_relative = relative.join(name);
            let portable = child_relative
                .to_str()
                .ok_or_else(|| Error::Invalid("build-tree path must be UTF-8".into()))?;
            validate_relative_path(portable)?;
            let stat = rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| Error::Io(error.into()))?;
            match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
                rustix::fs::FileType::Directory => {
                    let child = open_pinned_child(directory, name, true)?;
                    let mode = portable_mode(&child.metadata()?);
                    entries.push(LayoutEntry::Directory {
                        path: portable.to_owned(),
                        mode,
                    });
                    visit(&child, &child_relative, entries)?;
                }
                rustix::fs::FileType::RegularFile => {
                    let mut child = open_pinned_child(directory, name, false)?;
                    let metadata = child.metadata()?;
                    reject_hard_link(&metadata, portable)?;
                    let mode = portable_mode(&metadata);
                    let size_bytes = metadata.len();
                    let sha256 = sha256_reader(&mut child)?;
                    let final_metadata = child.metadata()?;
                    reject_hard_link(&final_metadata, portable)?;
                    if final_metadata.len() != size_bytes {
                        return Err(Error::Invalid(format!(
                            "build-tree file changed while reading {portable}"
                        )));
                    }
                    entries.push(LayoutEntry::File {
                        path: portable.to_owned(),
                        mode,
                        size_bytes,
                        sha256,
                    });
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "build tree contains link or special file {portable}"
                    )));
                }
            }
            if entries.len() > MAX_LAYOUT_ENTRIES {
                return Err(Error::Invalid(
                    "build tree exceeds the supported entry count".into(),
                ));
            }
        }
        Ok(())
    }

    let mut entries = Vec::new();
    visit(root, Path::new(""), &mut entries)?;
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    if entries.is_empty() {
        return Err(Error::Invalid("build tree must not be empty".into()));
    }
    Ok(entries)
}

#[cfg(unix)]
fn reject_hard_link(metadata: &fs::Metadata, path: &str) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        return Err(Error::Invalid(format!(
            "build tree contains hard-linked file {path}"
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn observe_pinned_build_tree(_root: &File) -> Result<Vec<LayoutEntry>, Error> {
    Err(Error::Invalid(
        "verified build-tree packing requires no-follow directory handles on this platform".into(),
    ))
}

#[cfg(unix)]
fn open_pinned_child(
    parent: &File,
    name: &std::ffi::OsStr,
    directory: bool,
) -> Result<File, Error> {
    use rustix::fs::{Mode, OFlags, openat};

    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    let file = File::from(
        openat(parent, name, flags, Mode::empty()).map_err(|error| Error::Io(error.into()))?,
    );
    let metadata = file.metadata()?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(Error::Invalid(
            "build-tree entry changed type while opening".into(),
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_pinned_entry(root: &File, relative: &Path) -> Result<File, Error> {
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value),
            _ => Err(Error::Invalid(
                "build-tree archive path is not normalized".into(),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut current = root.try_clone()?;
    for (index, component) in components.iter().enumerate() {
        current = open_pinned_child(&current, component, index + 1 != components.len())?;
    }
    Ok(current)
}

#[cfg(unix)]
fn portable_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn portable_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.is_dir() {
        0o755
    } else if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(unix)]
fn write_build_tree_archive(observed: &ObservedBuildTree, output: &File) -> Result<(), Error> {
    let output = output.try_clone()?;
    output.set_len(0)?;
    let encoder = zstd::stream::write::Encoder::new(output, 3)?;
    let mut archive = tar::Builder::new(encoder);
    for entry in &observed.entries {
        let mut header = tar::Header::new_ustar();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        match entry {
            LayoutEntry::Directory { path, mode } => {
                header.set_path(path).map_err(|error| {
                    Error::Invalid(format!(
                        "build-tree path cannot be represented without an archive extension record: {path}: {error}"
                    ))
                })?;
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(*mode);
                header.set_size(0);
                header.set_cksum();
                archive.append_data(&mut header, path, std::io::empty())?;
            }
            LayoutEntry::File {
                path,
                mode,
                size_bytes,
                ..
            } => {
                header.set_path(path).map_err(|error| {
                    Error::Invalid(format!(
                        "build-tree path cannot be represented without an archive extension record: {path}: {error}"
                    ))
                })?;
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(*mode);
                header.set_size(*size_bytes);
                header.set_cksum();
                let mut file = open_pinned_entry(&observed.root, Path::new(path))?;
                archive.append_data(&mut header, path, &mut file)?;
            }
        }
    }
    archive.finish()?;
    let encoder = archive.into_inner()?;
    let output = encoder.finish()?;
    output.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_build_tree_archive(_observed: &ObservedBuildTree, _output: &File) -> Result<(), Error> {
    Err(Error::Invalid(
        "verified build-tree packing requires no-follow directory handles on this platform".into(),
    ))
}

fn digest_archive_chunks(
    path: &Path,
    chunk_size_bytes: u64,
) -> Result<(String, u64, Vec<ArtifactChunk>), Error> {
    let chunk_size = usize::try_from(chunk_size_bytes)
        .map_err(|_| Error::Invalid("chunk size does not fit memory".into()))?;
    let mut file = File::open(path)?;
    let mut aggregate = Sha256::new();
    let mut buffer = vec![0_u8; chunk_size];
    let mut offset = 0_u64;
    let mut chunks = Vec::new();
    loop {
        let mut filled = 0_usize;
        while filled < buffer.len() {
            let read = file.read(&mut buffer[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            break;
        }
        aggregate.update(&buffer[..filled]);
        chunks.push(ArtifactChunk {
            offset,
            size_bytes: filled as u64,
            sha256: sha256_bytes(&buffer[..filled]),
        });
        offset = offset
            .checked_add(filled as u64)
            .ok_or_else(|| Error::Invalid("artifact size overflows u64".into()))?;
    }
    Ok((hex::encode(aggregate.finalize()), offset, chunks))
}

/// Read-only outcome of authenticating an interrupted partial artifact.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeDisposition {
    Restart,
    Append { verified_bytes: u64 },
    CompletePendingFinalVerification,
}

/// Revalidated content identity for the exact prefix retained by an applied
/// resume plan.
///
/// This value can only be obtained while the transfer lease is held and the
/// partial still matches the opaque plan. It is observation evidence, not
/// permission to invoke a transport.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct PreparedResumeEvidence {
    artifact_sha256: String,
    artifact_size_bytes: u64,
    manifest_sha256: String,
    session: String,
    disposition: ResumeDisposition,
    verified_prefix_bytes: u64,
    verified_prefix_sha256: String,
}

/// Payload counters parsed from one successful rsync `--stats` report.
///
/// Protocol overhead remains separate from artifact bytes. `literal_data_bytes`
/// is the transport-origin counter used for canary artifact-byte accounting.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct RsyncTransferStats {
    total_file_size_bytes: u64,
    total_transferred_file_size_bytes: u64,
    literal_data_bytes: u64,
    matched_data_bytes: u64,
    total_bytes_sent: u64,
    total_bytes_received: u64,
}

/// Exact payload-byte accounting bound to a previously authenticated prefix.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct ReceiverPullStatsEvidence {
    prepared: PreparedResumeEvidence,
    stats: RsyncTransferStats,
    artifact_bytes_total: u64,
    artifact_bytes_reused: u64,
    artifact_bytes_transferred: u64,
}

impl PreparedResumeEvidence {
    /// Manifest-bound complete encoded artifact size.
    #[must_use]
    pub fn artifact_size_bytes(&self) -> u64 {
        self.artifact_size_bytes
    }

    /// Applied resume behavior authenticated by this evidence.
    #[must_use]
    pub fn disposition(&self) -> ResumeDisposition {
        self.disposition
    }

    /// Exact number of authenticated prefix bytes retained.
    #[must_use]
    pub fn verified_prefix_bytes(&self) -> u64 {
        self.verified_prefix_bytes
    }

    /// SHA-256 of the complete retained prefix, including the empty prefix.
    #[must_use]
    pub fn verified_prefix_sha256(&self) -> &str {
        &self.verified_prefix_sha256
    }
}

impl RsyncTransferStats {
    /// Artifact payload bytes reported as literal transport data.
    #[must_use]
    pub fn literal_data_bytes(&self) -> u64 {
        self.literal_data_bytes
    }

    /// Total receiver-side bytes, including rsync protocol overhead.
    #[must_use]
    pub fn total_bytes_received(&self) -> u64 {
        self.total_bytes_received
    }
}

impl ReceiverPullStatsEvidence {
    /// Authenticated prefix state captured before receiver-pull.
    #[must_use]
    pub fn prepared(&self) -> &PreparedResumeEvidence {
        &self.prepared
    }

    /// Complete parsed rsync transport counters.
    #[must_use]
    pub fn stats(&self) -> &RsyncTransferStats {
        &self.stats
    }

    /// Manifest-bound encoded artifact size.
    #[must_use]
    pub fn artifact_bytes_total(&self) -> u64 {
        self.artifact_bytes_total
    }

    /// Authenticated prefix bytes reused by this pull.
    #[must_use]
    pub fn artifact_bytes_reused(&self) -> u64 {
        self.artifact_bytes_reused
    }

    /// Literal payload bytes newly transferred by this pull.
    #[must_use]
    pub fn artifact_bytes_transferred(&self) -> u64 {
        self.artifact_bytes_transferred
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResumeAction {
    Restart,
    Append { verified_bytes: u64 },
    CompletePendingFinalVerification,
}

/// An opaque resume decision bound to one manifest, lease, path, and observed file state.
///
/// Callers cannot construct or modify this value. It must be revalidated and
/// applied with [`apply_resume_plan`] before it can authorize receiver-pull
/// command construction.
#[derive(Debug)]
pub struct ResumePlan {
    artifact_sha256: String,
    manifest_sha256: String,
    session: String,
    partial_path: PathBuf,
    observed_length: u64,
    action: ResumeAction,
    prepared: bool,
    prepared_sha256: Option<String>,
}

impl ResumePlan {
    /// Describe the authenticated action without exposing mutable plan state.
    #[must_use]
    pub fn disposition(&self) -> ResumeDisposition {
        match self.action {
            ResumeAction::Restart => ResumeDisposition::Restart,
            ResumeAction::Append { verified_bytes } => ResumeDisposition::Append { verified_bytes },
            ResumeAction::CompletePendingFinalVerification => {
                ResumeDisposition::CompletePendingFinalVerification
            }
        }
    }
}

/// Revalidate an applied resume plan and capture its exact retained-prefix
/// identity immediately before receiver-pull command construction.
pub fn prepared_resume_evidence(
    transfer: &ArtifactTransferLease,
    manifest: &ArtifactManifest,
    plan: &ResumePlan,
) -> Result<PreparedResumeEvidence, Error> {
    manifest.validate()?;
    validate_transfer_binding(transfer, manifest)?;
    validate_prepared_resume(transfer, plan)?;
    let verified_prefix_sha256 = plan.prepared_sha256.clone().ok_or_else(|| {
        Error::Invalid("prepared resume plan has no authenticated content digest".into())
    })?;
    Ok(PreparedResumeEvidence {
        artifact_sha256: manifest.artifact_sha256.clone(),
        artifact_size_bytes: manifest.artifact_size_bytes,
        manifest_sha256: manifest.canonical_sha256()?,
        session: transfer.session.clone(),
        disposition: plan.disposition(),
        verified_prefix_bytes: plan.observed_length,
        verified_prefix_sha256,
    })
}

/// Parse the bounded stable fields emitted by one successful rsync/openrsync
/// `--stats` process result. The output is consumed so a failed process or a
/// detached raw stdout buffer cannot mint transport evidence.
pub fn parse_rsync_transfer_stats(
    output: std::process::Output,
) -> Result<RsyncTransferStats, Error> {
    const MAX_RSYNC_STATS_BYTES: usize = 16 * 1024;
    let std::process::Output {
        status,
        stdout,
        stderr: _,
    } = output;
    if !status.success() {
        return Err(Error::Invalid(
            "rsync stats require a successful process result".into(),
        ));
    }
    if stdout.len() > MAX_RSYNC_STATS_BYTES {
        return Err(Error::Invalid("rsync stats output is too large".into()));
    }
    let stdout = std::str::from_utf8(&stdout)
        .map_err(|_| Error::Invalid("rsync stats output is not UTF-8".into()))?;
    let mut total_file_size_bytes = None;
    let mut total_transferred_file_size_bytes = None;
    let mut literal_data_bytes = None;
    let mut matched_data_bytes = None;
    let mut total_bytes_sent = None;
    let mut total_bytes_received = None;
    for line in stdout.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let slot = match key.trim() {
            "Total file size" => &mut total_file_size_bytes,
            "Total transferred file size" => &mut total_transferred_file_size_bytes,
            "Literal data" => &mut literal_data_bytes,
            "Matched data" => &mut matched_data_bytes,
            "Total bytes sent" => &mut total_bytes_sent,
            "Total bytes received" => &mut total_bytes_received,
            _ => continue,
        };
        if slot.is_some() {
            return Err(Error::Invalid(format!(
                "rsync stats contains duplicate {key} counter"
            )));
        }
        let numeric = value.trim().strip_suffix(" bytes").unwrap_or(value.trim());
        *slot =
            Some(parse_rsync_decimal(numeric).map_err(|()| {
                Error::Invalid(format!("rsync stats has malformed {key} counter"))
            })?);
    }
    let missing =
        |field: &'static str| Error::Invalid(format!("rsync stats is missing {field} counter"));
    Ok(RsyncTransferStats {
        total_file_size_bytes: total_file_size_bytes.ok_or_else(|| missing("total file size"))?,
        total_transferred_file_size_bytes: total_transferred_file_size_bytes
            .ok_or_else(|| missing("total transferred file size"))?,
        literal_data_bytes: literal_data_bytes.ok_or_else(|| missing("literal data"))?,
        matched_data_bytes: matched_data_bytes.ok_or_else(|| missing("matched data"))?,
        total_bytes_sent: total_bytes_sent.ok_or_else(|| missing("total bytes sent"))?,
        total_bytes_received: total_bytes_received
            .ok_or_else(|| missing("total bytes received"))?,
    })
}

fn parse_rsync_decimal(value: &str) -> Result<u64, ()> {
    if !value.contains(',') {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(());
        }
        return value.parse().map_err(|_| ());
    }
    let mut groups = value.split(',');
    let first = groups.next().ok_or(())?;
    if first.is_empty() || first.len() > 3 || !first.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    let mut normalized = first.to_owned();
    let mut saw_group = false;
    for group in groups {
        saw_group = true;
        if group.len() != 3 || !group.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(());
        }
        normalized.push_str(group);
    }
    if !saw_group {
        return Err(());
    }
    normalized.parse().map_err(|_| ())
}

/// Bind parsed transport-origin counters to an authenticated pre-transfer
/// prefix. This does not claim final artifact verification or publication.
pub fn bind_receiver_pull_stats(
    prepared: PreparedResumeEvidence,
    stats: RsyncTransferStats,
) -> Result<ReceiverPullStatsEvidence, Error> {
    let artifact_bytes_total = prepared.artifact_size_bytes;
    let prefix_matches_disposition = match prepared.disposition {
        ResumeDisposition::Restart => prepared.verified_prefix_bytes == 0,
        ResumeDisposition::Append { verified_bytes } => {
            verified_bytes != 0 && verified_bytes == prepared.verified_prefix_bytes
        }
        ResumeDisposition::CompletePendingFinalVerification => false,
    };
    if prepared.artifact_sha256.is_empty()
        || prepared.manifest_sha256.is_empty()
        || prepared.session.is_empty()
        || !prefix_matches_disposition
        || artifact_bytes_total == 0
        || prepared.verified_prefix_bytes > artifact_bytes_total
        || stats.total_file_size_bytes != artifact_bytes_total
        || stats.matched_data_bytes != 0
    {
        return Err(Error::Invalid(
            "rsync stats do not match the authenticated transfer prefix".into(),
        ));
    }
    let remaining = artifact_bytes_total - prepared.verified_prefix_bytes;
    if stats.literal_data_bytes != remaining
        || stats.total_transferred_file_size_bytes < stats.literal_data_bytes
        || stats.total_transferred_file_size_bytes > artifact_bytes_total
        || stats.total_bytes_received < stats.literal_data_bytes
    {
        return Err(Error::Invalid(
            "rsync stats do not prove exact receiver-pull payload bytes".into(),
        ));
    }
    Ok(ReceiverPullStatsEvidence {
        artifact_bytes_total,
        artifact_bytes_reused: prepared.verified_prefix_bytes,
        artifact_bytes_transferred: stats.literal_data_bytes,
        prepared,
        stats,
    })
}

/// Authenticate full chunks and bind the only safe resume action to an active transfer lease.
pub fn plan_verified_resume(
    transfer: &ArtifactTransferLease,
    manifest: &ArtifactManifest,
) -> Result<ResumePlan, Error> {
    manifest.validate()?;
    validate_transfer_binding(transfer, manifest)?;
    let path = transfer.partial_path();
    let (length, action) = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(Error::Invalid(
                    "artifact partial must be a regular non-symlink file".into(),
                ));
            }
            let mut file = File::open(path)?;
            let length = file.metadata()?.len();
            (length, plan_resume_action(&mut file, length, manifest)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            reject_existing_sealed_transfer(transfer)?;
            (0, ResumeAction::Restart)
        }
        Err(error) => return Err(error.into()),
    };
    Ok(ResumePlan {
        artifact_sha256: manifest.artifact_sha256.clone(),
        manifest_sha256: manifest.canonical_sha256()?,
        session: transfer.session.clone(),
        partial_path: path.to_path_buf(),
        observed_length: length,
        action,
        prepared: false,
        prepared_sha256: None,
    })
}

fn plan_resume_action(
    file: &mut File,
    length: u64,
    manifest: &ArtifactManifest,
) -> Result<ResumeAction, Error> {
    if length > manifest.artifact_size_bytes {
        return Ok(ResumeAction::Restart);
    }
    let mut verified = 0_u64;
    for chunk in &manifest.chunks {
        if length < chunk.offset + chunk.size_bytes {
            break;
        }
        file.seek(SeekFrom::Start(chunk.offset))?;
        let mut take = Read::by_ref(file).take(chunk.size_bytes);
        let digest = sha256_reader(&mut take)?;
        if digest != chunk.sha256 {
            return if verified == 0 {
                Ok(ResumeAction::Restart)
            } else {
                Ok(ResumeAction::Append {
                    verified_bytes: verified,
                })
            };
        }
        verified += chunk.size_bytes;
    }
    if verified == manifest.artifact_size_bytes && length == verified {
        Ok(ResumeAction::CompletePendingFinalVerification)
    } else if verified == 0 {
        Ok(ResumeAction::Restart)
    } else {
        Ok(ResumeAction::Append {
            verified_bytes: verified,
        })
    }
}

/// Revalidate and apply an opaque plan, returning the prepared value required by receiver-pull.
pub fn apply_resume_plan(
    transfer: &ArtifactTransferLease,
    manifest: &ArtifactManifest,
    mut plan: ResumePlan,
) -> Result<ResumePlan, Error> {
    manifest.validate()?;
    validate_resume_binding(transfer, manifest, &plan)?;
    let mut file = match fs::symlink_metadata(transfer.partial_path()) {
        Ok(_) => {
            reject_non_regular_file(transfer.partial_path(), "artifact partial")?;
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(transfer.partial_path())?
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && plan.observed_length == 0
                && plan.action == ResumeAction::Restart =>
        {
            reject_existing_sealed_transfer(transfer)?;
            OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(transfer.partial_path())
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        Error::Invalid("artifact partial appeared after resume planning".into())
                    } else {
                        Error::Io(error)
                    }
                })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Invalid(
                "artifact partial disappeared after resume planning".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let current_length = file.metadata()?.len();
    let current_action = plan_resume_action(&mut file, current_length, manifest)?;
    if current_length != plan.observed_length || current_action != plan.action {
        return Err(Error::Invalid(
            "artifact partial changed after resume planning".into(),
        ));
    }
    let prepared_length = match plan.action {
        ResumeAction::Restart => 0,
        ResumeAction::Append { verified_bytes } => verified_bytes,
        ResumeAction::CompletePendingFinalVerification => current_length,
    };
    file.set_len(prepared_length)?;
    file.sync_all()?;
    plan.observed_length = prepared_length;
    plan.prepared = true;
    file.seek(SeekFrom::Start(0))?;
    plan.prepared_sha256 = Some(sha256_reader(&mut file)?);
    Ok(plan)
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
    /// Exclusive receiver-side ownership retained through publication.
    pub transfer: &'a ArtifactTransferLease,
    pub resume: &'a ResumePlan,
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
        transfer,
        resume,
        timeout_seconds,
    } = request;
    validate_prepared_resume(transfer, resume)?;
    if !rsync_program.is_absolute() || rsync_program.file_name().is_none() {
        return Err(Error::Invalid(
            "rsync program must be an absolute path".into(),
        ));
    }
    validate_host(remote_host)?;
    let root = portable_absolute_path(remote_store_root)?;
    let artifact_sha256 = transfer.artifact_sha256();
    let local_partial = transfer.partial_path().to_path_buf();
    if transfer.sealed_path.exists() {
        reject_non_regular_file(&transfer.sealed_path, "sealed artifact")?;
        return Err(Error::Invalid(
            "sealed artifact must be published, not transferred again".into(),
        ));
    }
    if local_partial.exists() {
        reject_non_regular_file(&local_partial, "artifact partial")?;
    }
    if *timeout_seconds == 0 {
        return Err(Error::Invalid("rsync timeout must be non-zero".into()));
    }
    let remote = format!("{remote_host}:{root}/objects/{artifact_sha256}.tar.zst");
    let mut args = vec![OsString::from("-a"), OsString::from("--partial")];
    match resume.action {
        ResumeAction::Append { verified_bytes } if verified_bytes > 0 => {
            args.push(OsString::from("--append"));
        }
        ResumeAction::Append { .. } => {
            return Err(Error::Invalid(
                "append requires a non-zero verified boundary".into(),
            ));
        }
        ResumeAction::Restart => {}
        ResumeAction::CompletePendingFinalVerification => {
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

fn validate_transfer_binding(
    transfer: &ArtifactTransferLease,
    manifest: &ArtifactManifest,
) -> Result<(), Error> {
    if transfer.artifact_sha256 != manifest.artifact_sha256 {
        return Err(Error::Invalid(
            "resume manifest does not match the transfer lease digest".into(),
        ));
    }
    Ok(())
}

fn reject_existing_sealed_transfer(transfer: &ArtifactTransferLease) -> Result<(), Error> {
    match fs::symlink_metadata(&transfer.sealed_path) {
        Ok(_) => Err(Error::Invalid(
            "sealed artifact must be published or recovered before receiving again".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_resume_binding(
    transfer: &ArtifactTransferLease,
    manifest: &ArtifactManifest,
    plan: &ResumePlan,
) -> Result<(), Error> {
    validate_transfer_binding(transfer, manifest)?;
    if plan.prepared
        || plan.artifact_sha256 != manifest.artifact_sha256
        || plan.manifest_sha256 != manifest.canonical_sha256()?
        || plan.session != transfer.session
        || plan.partial_path != transfer.partial_path
    {
        return Err(Error::Invalid(
            "resume plan is stale or belongs to a different transfer".into(),
        ));
    }
    Ok(())
}

fn validate_prepared_resume(
    transfer: &ArtifactTransferLease,
    plan: &ResumePlan,
) -> Result<(), Error> {
    if !plan.prepared
        || plan.artifact_sha256 != transfer.artifact_sha256
        || plan.session != transfer.session
        || plan.partial_path != transfer.partial_path
    {
        return Err(Error::Invalid(
            "receiver pull requires a prepared plan for this exact transfer".into(),
        ));
    }
    reject_non_regular_file(transfer.partial_path(), "artifact partial")?;
    if fs::metadata(transfer.partial_path())?.len() != plan.observed_length {
        return Err(Error::Invalid(
            "artifact partial changed after resume preparation".into(),
        ));
    }
    let expected_digest = plan.prepared_sha256.as_deref().ok_or_else(|| {
        Error::Invalid("prepared resume plan has no authenticated content digest".into())
    })?;
    let mut partial = File::open(transfer.partial_path())?;
    if sha256_reader(&mut partial)? != expected_digest {
        return Err(Error::Invalid(
            "artifact partial content changed after resume preparation".into(),
        ));
    }
    Ok(())
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

/// Exclusive ownership of one digest/session receiver staging path.
///
/// The lease is process-independent: an OS file lock releases automatically if
/// its owner exits. Callers keep the same value alive while planning/resuming a
/// receiver pull and pass it into publication so two cooperating Shipyard
/// processes cannot write or publish the same staging path concurrently.
pub struct ArtifactTransferLease {
    store_root: PathBuf,
    artifact_sha256: String,
    session: String,
    partial_path: PathBuf,
    sealed_path: PathBuf,
    lock_file: File,
}

impl ArtifactTransferLease {
    /// Digest whose staging path this lease exclusively owns.
    #[must_use]
    pub fn artifact_sha256(&self) -> &str {
        &self.artifact_sha256
    }

    /// Transfer-session identifier whose staging path this lease owns.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Receiver destination for the resumable partial.
    #[must_use]
    pub fn partial_path(&self) -> &Path {
        &self.partial_path
    }
}

impl Drop for ArtifactTransferLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
    }
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

    /// Acquire exclusive ownership of one digest/session transfer path.
    ///
    /// Lock files deliberately remain on disk after release: unlinking a lock
    /// pathname can split concurrent lockers across two inodes. The kernel lock
    /// itself is released automatically on process death, so an offline or
    /// crashed receiver cannot permanently wedge the session.
    pub fn acquire_transfer_lease(
        &self,
        digest: &str,
        session: &str,
    ) -> Result<ArtifactTransferLease, Error> {
        let partial_path = self.partial_path(digest, session)?;
        let sealed_path = self
            .root
            .join(".incoming")
            .join(format!("{digest}.{session}.sealed"));
        let lock_path = self
            .root
            .join(".incoming")
            .join(format!("{digest}.{session}.lease"));
        reject_symlink(&lock_path)?;
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        reject_non_regular_file(&lock_path, "artifact transfer lease")?;
        FileExt::try_lock_exclusive(&lock_file).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                Error::Invalid(format!(
                    "artifact transfer {digest}/{session} is already leased"
                ))
            } else {
                Error::Io(error)
            }
        })?;
        Ok(ArtifactTransferLease {
            store_root: self.root.clone(),
            artifact_sha256: digest.to_owned(),
            session: session.to_owned(),
            partial_path,
            sealed_path,
            lock_file,
        })
    }

    /// Verify exact manifest authority, bytes, and chunks, then atomically publish.
    pub fn publish_verified(
        &self,
        manifest: &ArtifactManifest,
        authority: &ManifestAuthority,
        transfer: ArtifactTransferLease,
    ) -> Result<PublishOutcome, Error> {
        self.publish_verified_with_hook(manifest, authority, transfer, |_| Ok(()))
    }

    fn publish_verified_with_hook(
        &self,
        manifest: &ArtifactManifest,
        authority: &ManifestAuthority,
        transfer: ArtifactTransferLease,
        before_publish: impl FnOnce(&Path) -> Result<(), Error>,
    ) -> Result<PublishOutcome, Error> {
        manifest.validate_authority(authority)?;
        if transfer.store_root != self.root || transfer.artifact_sha256 != manifest.artifact_sha256
        {
            return Err(Error::Invalid(
                "artifact transfer lease does not belong to this store and digest".into(),
            ));
        }
        let sealed = Self::seal_transfer(&transfer)?;
        if let Err(validation_error) = verify_sealed_artifact(&sealed, manifest) {
            if let Err(recovery_error) = Self::restore_partial_after_validation_failure(&transfer) {
                return Err(Error::Invalid(format!(
                    "{validation_error}; failed to restore resumable partial: {recovery_error}"
                )));
            }
            return Err(validation_error);
        }
        let destination = self
            .root
            .join("objects")
            .join(format!("{}.tar.zst", manifest.artifact_sha256));
        before_publish(&destination)?;
        let outcome = match fs::hard_link(&sealed, &destination) {
            Ok(()) => {
                sync_directory(destination.parent().expect("destination has parent"))?;
                PublishOutcome::Published(destination)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::verify_existing_object(&destination, manifest)?;
                sync_directory(destination.parent().expect("destination has parent"))?;
                PublishOutcome::Reused(destination)
            }
            Err(error) => return Err(error.into()),
        };
        fs::remove_file(&sealed)?;
        // The object-directory entry is already durable. A crash before this
        // cleanup directory sync can only resurrect a verified sealed link;
        // the retry path safely verifies/reuses the immutable object.
        let _ = sync_directory(sealed.parent().expect("sealed artifact has parent"));
        // Publication deliberately consumes the lease so its caller cannot
        // accidentally resume a receiver with stale ownership afterward.
        drop(transfer);
        Ok(outcome)
    }

    fn seal_transfer(transfer: &ArtifactTransferLease) -> Result<PathBuf, Error> {
        match (
            transfer.partial_path.exists(),
            transfer.sealed_path.exists(),
        ) {
            (true, true) => Err(Error::Invalid(
                "artifact transfer has both partial and sealed state".into(),
            )),
            (false, false) => Err(Error::Invalid(
                "artifact transfer has no partial or sealed state".into(),
            )),
            (false, true) => {
                reject_non_regular_file(&transfer.sealed_path, "sealed artifact")?;
                Ok(transfer.sealed_path.clone())
            }
            (true, false) => {
                reject_non_regular_file(&transfer.partial_path, "artifact partial")?;
                reject_symlink(&transfer.sealed_path)?;
                fs::rename(&transfer.partial_path, &transfer.sealed_path)?;
                sync_directory(
                    transfer
                        .sealed_path
                        .parent()
                        .expect("sealed artifact has parent"),
                )?;
                Ok(transfer.sealed_path.clone())
            }
        }
    }

    fn restore_partial_after_validation_failure(
        transfer: &ArtifactTransferLease,
    ) -> Result<(), Error> {
        if transfer.partial_path.exists() {
            return Err(Error::Invalid(
                "cannot restore sealed artifact over an existing partial".into(),
            ));
        }
        reject_non_regular_file(&transfer.sealed_path, "sealed artifact")?;
        fs::rename(&transfer.sealed_path, &transfer.partial_path)?;
        sync_directory(
            transfer
                .partial_path
                .parent()
                .expect("artifact partial has parent"),
        )?;
        Ok(())
    }

    fn verify_existing_object(
        destination: &Path,
        manifest: &ArtifactManifest,
    ) -> Result<(), Error> {
        let existing = fs::symlink_metadata(destination)?;
        if !existing.file_type().is_file()
            || existing.file_type().is_symlink()
            || existing.len() != manifest.artifact_size_bytes
        {
            return Err(Error::Invalid(
                "published artifact path is not the expected immutable object".into(),
            ));
        }
        let mut existing_file = File::open(destination)?;
        if sha256_reader(&mut existing_file)? != manifest.artifact_sha256 {
            return Err(Error::Invalid(
                "published artifact digest conflicts with manifest".into(),
            ));
        }
        Ok(())
    }
}

fn verify_sealed_artifact(path: &Path, manifest: &ArtifactManifest) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid(
            "sealed artifact must be a regular non-symlink file".into(),
        ));
    }
    if metadata.len() != manifest.artifact_size_bytes {
        return Err(Error::Invalid("sealed artifact has the wrong size".into()));
    }
    let mut chunk_file = File::open(path)?;
    if plan_resume_action(&mut chunk_file, metadata.len(), manifest)?
        != ResumeAction::CompletePendingFinalVerification
    {
        return Err(Error::Invalid(
            "sealed artifact failed chunk verification".into(),
        ));
    }
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    if sha256_reader(&mut file)? != manifest.artifact_sha256 {
        return Err(Error::Invalid("artifact final digest mismatch".into()));
    }
    file.sync_all()?;
    Ok(())
}

/// Verify encoded bytes and require the archive's complete unpacked layout to
/// match the manifest exactly. Nothing is extracted by this operation.
pub fn verify_archive_layout(path: &Path, manifest: &ArtifactManifest) -> Result<(), Error> {
    manifest.validate()?;
    verify_encoded_artifact(path, manifest)?;
    scan_archive(path, manifest, None)?;
    // Detect replacement or mutation while the archive was decoded.
    verify_encoded_artifact(path, manifest)
}

/// Verify an archive, safely extract it into private sibling staging, then
/// atomically publish the complete unpacked tree at `destination`.
///
/// The destination must not exist. Traversal, links, special files, duplicate
/// paths, undeclared paths, and any type/mode/size/digest mismatch fail closed.
pub fn extract_verified_archive(
    path: &Path,
    manifest: &ArtifactManifest,
    authority: &ManifestAuthority,
    destination: &Path,
    space_policy: SpacePolicy,
) -> Result<PublicationOutcome, Error> {
    // Archive hashes prove byte identity, not that the current scheduler still
    // authorizes this source head and producer lease. Bind consumption to the
    // same exact authority fence required for object publication.
    manifest.validate_authority(authority)?;
    verify_archive_layout(path, manifest)?;
    let parent = validate_restore_destination(destination)?;
    if destination.exists() {
        return Err(Error::Invalid(
            "artifact extraction destination already exists".into(),
        ));
    }
    let _lock = acquire_extraction_lock(parent)?;
    extract_verified_archive_locked(path, manifest, authority, destination, space_policy)
}

fn acquire_extraction_lock(parent: &Path) -> Result<File, Error> {
    let extraction_lock_path = parent.join(".shipyard-artifact-extract.lease");
    reject_symlink(&extraction_lock_path)?;
    let extraction_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&extraction_lock_path)?;
    reject_non_regular_file(&extraction_lock_path, "artifact extraction lease")?;
    FileExt::try_lock_exclusive(&extraction_lock).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Error::Invalid("another artifact extraction owns this destination parent".into())
        } else {
            Error::Io(error)
        }
    })?;
    Ok(extraction_lock)
}

fn extract_verified_archive_locked(
    path: &Path,
    manifest: &ArtifactManifest,
    authority: &ManifestAuthority,
    destination: &Path,
    space_policy: SpacePolicy,
) -> Result<PublicationOutcome, Error> {
    manifest.validate_authority(authority)?;
    verify_archive_layout(path, manifest)?;
    let parent = validate_restore_destination(destination)?;
    if destination.exists() {
        return Err(Error::Invalid(
            "artifact extraction destination already exists".into(),
        ));
    }
    let allocation_budget = manifest.unpacked_allocation_budget_bytes()?;
    space_policy.check_path(parent, allocation_budget, 0)?;
    let staging = tempfile::Builder::new()
        .prefix(".shipyard-artifact-")
        .tempdir_in(parent)?;
    let mut space_probe = |probe_path: &Path| available_space(probe_path).map_err(Error::Io);
    let mut extraction = ExtractionContext {
        root: staging.path(),
        space_policy,
        remaining_bytes: allocation_budget,
        space_probe: &mut space_probe,
    };
    let mut directory_modes = scan_archive(path, manifest, Some(&mut extraction))?;
    if let Some(root_mode) = manifest.root_mode {
        directory_modes.push((extraction.root.to_path_buf(), root_mode));
    }
    extraction
        .space_policy
        .check_path(extraction.root, extraction.remaining_bytes, 0)?;
    // A mutable source cannot become trusted by changing between the proof and
    // extraction pass. The private staging tree is discarded on mismatch.
    verify_encoded_artifact(path, manifest)?;
    publish_staging_no_replace(staging, destination, directory_modes)?;
    Ok(publication_outcome(destination, parent, sync_directory))
}

fn publication_outcome(
    destination: &Path,
    parent: &Path,
    sync: impl FnOnce(&Path) -> Result<(), Error>,
) -> PublicationOutcome {
    match sync(parent) {
        Ok(()) => PublicationOutcome::Durable,
        Err(error) => PublicationOutcome::PublishedParentSyncPending {
            destination: destination.to_path_buf(),
            message: error.to_string(),
        },
    }
}

fn publish_staging_no_replace(
    staging: tempfile::TempDir,
    destination: &Path,
    mut directory_modes: Vec<(PathBuf, u32)>,
) -> Result<(), Error> {
    // Open every directory while the private tree is still traversable. Some
    // declared final modes intentionally remove read/search permission, but an
    // already-open handle can still durably flush the final metadata and child
    // entries before the tree is made visible.
    let directory_handles = open_directory_handles_bottom_up(staging.path())?;
    if let Err(error) = apply_directory_modes(&mut directory_modes) {
        restore_directory_modes_for_cleanup(&mut directory_modes);
        return Err(error);
    }
    if let Err(error) = sync_directory_handles(directory_handles) {
        restore_directory_modes_for_cleanup(&mut directory_modes);
        return Err(error);
    }
    let staging_path = staging.keep();
    if let Err(error) = rename_no_replace(&staging_path, destination) {
        restore_directory_modes_for_cleanup(&mut directory_modes);
        let _ = fs::remove_dir_all(&staging_path);
        return Err(error);
    }
    Ok(())
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "redox"
))]
fn rename_no_replace(source: &Path, destination: &Path) -> Result<(), Error> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE)
        .map_err(|error| Error::Io(error.into()))
}

#[cfg(all(
    unix,
    not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    ))
))]
fn rename_no_replace(_source: &Path, _destination: &Path) -> Result<(), Error> {
    // POSIX rename may overwrite the destination. Targets without a native
    // atomic no-replace primitive must fail closed instead of weakening the
    // immutable publication contract with an existence-check race.
    Err(Error::Invalid(
        "atomic no-replace artifact publication is unsupported on this platform".into(),
    ))
}

#[cfg(windows)]
fn rename_no_replace(source: &Path, destination: &Path) -> Result<(), Error> {
    atomicwrites::move_atomic(source, destination).map_err(Error::Io)
}

fn verify_encoded_artifact(path: &Path, manifest: &ArtifactManifest) -> Result<(), Error> {
    reject_non_regular_file(path, "artifact archive")?;
    let metadata = fs::metadata(path)?;
    if metadata.len() != manifest.artifact_size_bytes {
        return Err(Error::Invalid("artifact archive has the wrong size".into()));
    }
    let mut file = File::open(path)?;
    if sha256_reader(&mut file)? != manifest.artifact_sha256 {
        return Err(Error::Invalid("artifact archive digest mismatch".into()));
    }
    Ok(())
}

fn scan_archive(
    path: &Path,
    manifest: &ArtifactManifest,
    mut extraction: Option<&mut ExtractionContext<'_>>,
) -> Result<Vec<(PathBuf, u32)>, Error> {
    let file = File::open(path)?;
    let mut decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| Error::Invalid(format!("invalid zstd artifact: {error}")))?;
    decoder
        .window_log_max(MAX_ZSTD_WINDOW_LOG)
        .map_err(|error| Error::Invalid(format!("invalid zstd artifact window: {error}")))?;
    let mut archive = tar::Archive::new(decoder);
    let expected: HashMap<&str, &LayoutEntry> = manifest
        .entries
        .iter()
        .map(|entry| (entry.path(), entry))
        .collect();
    let mut seen = HashSet::with_capacity(expected.len());
    let mut directory_modes = Vec::new();
    for entry in archive
        .entries()
        .map_err(|error| Error::Invalid(format!("invalid tar artifact: {error}")))?
        .raw(true)
    {
        let mut entry =
            entry.map_err(|error| Error::Invalid(format!("invalid tar entry: {error}")))?;
        let entry_type = entry.header().entry_type();
        if !entry_type.is_dir() && entry_type != tar::EntryType::Regular {
            return Err(Error::Invalid(
                "archive contains links, special files, or extension records".into(),
            ));
        }
        let entry_path = entry
            .path()
            .map_err(|error| Error::Invalid(format!("invalid tar path: {error}")))?;
        let raw_path = entry_path
            .to_str()
            .ok_or_else(|| Error::Invalid("archive path must be UTF-8".into()))?
            .to_owned();
        let path_string = if entry_type.is_dir() {
            raw_path.trim_end_matches('/').to_owned()
        } else {
            raw_path
        };
        validate_relative_path(&path_string)?;
        if !seen.insert(path_string.clone()) {
            return Err(Error::Invalid(format!(
                "archive contains duplicate path {path_string}"
            )));
        }
        let expected_entry = expected.get(path_string.as_str()).ok_or_else(|| {
            Error::Invalid(format!("archive contains undeclared path {path_string}"))
        })?;
        let mode = entry
            .header()
            .mode()
            .map_err(|error| Error::Invalid(format!("invalid mode for {path_string}: {error}")))?;
        validate_mode(mode)?;
        scan_layout_entry(
            &mut entry,
            &path_string,
            entry_type,
            mode,
            expected_entry,
            extraction.as_deref_mut(),
            &mut directory_modes,
        )?;
    }
    if seen.len() != expected.len() {
        let missing = manifest
            .entries
            .iter()
            .find(|entry| !seen.contains(entry.path()))
            .map_or("unknown", LayoutEntry::path);
        return Err(Error::Invalid(format!(
            "archive layout is incomplete; missing {missing}"
        )));
    }
    reject_trailing_archive_data(archive.into_inner())?;
    Ok(directory_modes)
}

fn apply_directory_modes(directory_modes: &mut [(PathBuf, u32)]) -> Result<(), Error> {
    directory_modes.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, mode) in directory_modes {
        set_portable_permissions(path, *mode)?;
    }
    Ok(())
}

fn restore_directory_modes_for_cleanup(directory_modes: &mut [(PathBuf, u32)]) {
    directory_modes.sort_by_key(|(path, _)| path.components().count());
    for (path, _) in directory_modes {
        let _ = set_portable_permissions(path, 0o700);
    }
}

struct ExtractionContext<'a> {
    root: &'a Path,
    space_policy: SpacePolicy,
    remaining_bytes: u64,
    space_probe: &'a mut dyn FnMut(&Path) -> Result<u64, Error>,
}

impl ExtractionContext<'_> {
    fn check_space(&mut self) -> Result<(), Error> {
        self.space_policy
            .check((self.space_probe)(self.root)?, self.remaining_bytes, 0)
    }

    fn ensure_directories(&mut self, relative: &Path) -> Result<(), Error> {
        let mut output = self.root.to_path_buf();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                return Err(Error::Invalid(
                    "extraction directory path is not normalized".into(),
                ));
            };
            output.push(component);
            if output.exists() {
                if !output.is_dir() {
                    return Err(Error::Invalid(
                        "artifact extraction path collides with a non-directory".into(),
                    ));
                }
                continue;
            }
            self.check_space()?;
            fs::create_dir(&output)?;
            self.remaining_bytes = self
                .remaining_bytes
                .checked_sub(ENTRY_ALLOCATION_RESERVE_BYTES)
                .ok_or_else(|| Error::Invalid("unpacked byte budget underflows".into()))?;
            self.check_space()?;
        }
        Ok(())
    }
}

fn scan_layout_entry<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    path: &str,
    entry_type: tar::EntryType,
    mode: u32,
    expected: &LayoutEntry,
    extraction: Option<&mut ExtractionContext<'_>>,
    directory_modes: &mut Vec<(PathBuf, u32)>,
) -> Result<(), Error> {
    match expected {
        LayoutEntry::Directory {
            mode: expected_mode,
            ..
        } => {
            if !entry_type.is_dir() || mode != *expected_mode || entry.size() != 0 {
                return Err(Error::Invalid(format!(
                    "archive directory type, mode, or size mismatch for {path}"
                )));
            }
            if let Some(extraction) = extraction {
                extraction.check_space()?;
                let output = extraction.root.join(path);
                extraction.ensure_directories(Path::new(path))?;
                directory_modes.push((output, *expected_mode));
                extraction.check_space()?;
            }
        }
        LayoutEntry::File {
            mode: expected_mode,
            size_bytes,
            sha256,
            ..
        } => {
            if entry_type != tar::EntryType::Regular
                || mode != *expected_mode
                || entry.size() != *size_bytes
            {
                return Err(Error::Invalid(format!(
                    "archive file type, mode, or size mismatch for {path}"
                )));
            }
            let mut hasher = Sha256::new();
            let copied = if let Some(extraction) = extraction {
                extraction.check_space()?;
                let output = extraction.root.join(path);
                if let Some(parent) = output.parent() {
                    let relative_parent = parent.strip_prefix(extraction.root).map_err(|_| {
                        Error::Invalid("artifact extraction parent escaped staging".into())
                    })?;
                    extraction.ensure_directories(relative_parent)?;
                }
                let mut output_file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&output)?;
                let copied = copy_and_hash(entry, &mut output_file, &mut hasher)?;
                set_portable_permissions(&output, *expected_mode)?;
                // Persist both bytes and the final executable/permission mode
                // before the containing directory can be published.
                output_file.sync_all()?;
                extraction.remaining_bytes = extraction
                    .remaining_bytes
                    .checked_sub(
                        size_bytes
                            .checked_add(ENTRY_ALLOCATION_RESERVE_BYTES)
                            .ok_or_else(|| {
                                Error::Invalid("unpacked byte budget overflows".into())
                            })?,
                    )
                    .ok_or_else(|| Error::Invalid("unpacked byte budget underflows".into()))?;
                extraction.check_space()?;
                copied
            } else {
                copy_and_hash(entry, &mut std::io::sink(), &mut hasher)?
            };
            if copied != *size_bytes {
                return Err(Error::Invalid(format!(
                    "archive file length changed while reading {path}"
                )));
            }
            if hex::encode(hasher.finalize()) != *sha256 {
                return Err(Error::Invalid(format!(
                    "archive file digest mismatch for {path}"
                )));
            }
        }
    }
    Ok(())
}

fn reject_trailing_archive_data(mut decoder: impl Read) -> Result<(), Error> {
    let mut trailing = [0_u8; 1024];
    let mut trailing_bytes = 0_usize;
    loop {
        let read = decoder.read(&mut trailing)?;
        if read == 0 {
            break;
        }
        trailing_bytes = trailing_bytes.saturating_add(read);
        if trailing_bytes > MAX_TAR_ZERO_PADDING_BYTES
            || trailing[..read].iter().any(|byte| *byte != 0)
        {
            return Err(Error::Invalid(
                "archive contains data after the terminal tar record".into(),
            ));
        }
    }
    Ok(())
}

fn copy_and_hash(
    reader: &mut impl Read,
    writer: &mut impl Write,
    hasher: &mut Sha256,
) -> Result<u64, Error> {
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| Error::Invalid("archive file size overflows u64".into()))?;
    }
    Ok(total)
}

#[cfg(unix)]
fn set_portable_permissions(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn set_portable_permissions(_path: &Path, _mode: u32) -> Result<(), Error> {
    Ok(())
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
        || value.len() > MAX_LAYOUT_PATH_BYTES
        || value.split('/').count() > MAX_LAYOUT_PATH_DEPTH
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
            "layout path is empty, oversized, too deep, or non-portable".into(),
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

fn validate_portable_component(component: &str) -> Result<(), Error> {
    if component.len() > MAX_LAYOUT_COMPONENT_BYTES {
        return Err(Error::Invalid(
            "layout path component exceeds the portable byte limit".into(),
        ));
    }
    if component.ends_with('.') {
        return Err(Error::Invalid(
            "layout path component has a Windows-normalized trailing period".into(),
        ));
    }
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper.strip_prefix("COM").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || upper.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        });
    if reserved {
        return Err(Error::Invalid(
            "layout path component is a reserved Windows device name".into(),
        ));
    }
    Ok(())
}

fn portable_absolute_path(path: &Path) -> Result<String, Error> {
    let value = path
        .to_str()
        .ok_or_else(|| Error::Invalid("remote store root must be UTF-8".into()))?;
    if !value.starts_with('/')
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
#[allow(clippy::unnecessary_wraps)] // Keep one fallible cross-platform durability contract.
fn sync_directory(_path: &Path) -> Result<(), Error> {
    Ok(())
}

#[cfg(unix)]
fn open_directory_handles_bottom_up(root: &Path) -> Result<Vec<File>, Error> {
    fn visit(path: &Path, handles: &mut Vec<File>) -> Result<(), Error> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                visit(&entry.path(), handles)?;
            }
        }
        handles.push(File::open(path)?);
        Ok(())
    }

    let mut handles = Vec::new();
    visit(root, &mut handles)?;
    Ok(handles)
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn open_directory_handles_bottom_up(_root: &Path) -> Result<Vec<File>, Error> {
    Ok(Vec::new())
}

fn sync_directory_handles(handles: Vec<File>) -> Result<(), Error> {
    for handle in handles {
        handle.sync_all()?;
    }
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

    #[cfg(unix)]
    fn rsync_output(success: bool, stdout: Vec<u8>) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;

        std::process::Output {
            status: std::process::ExitStatus::from_raw(if success { 0 } else { 1 << 8 }),
            stdout,
            stderr: Vec::new(),
        }
    }

    #[cfg(windows)]
    fn rsync_output(success: bool, stdout: Vec<u8>) -> std::process::Output {
        use std::os::windows::process::ExitStatusExt;

        std::process::Output {
            status: std::process::ExitStatus::from_raw(u32::from(!success)),
            stdout,
            stderr: Vec::new(),
        }
    }

    fn fixture(bytes: &[u8]) -> ArtifactManifest {
        let entries = vec![
            LayoutEntry::Directory {
                path: "bin".into(),
                mode: 0o755,
            },
            LayoutEntry::File {
                path: "bin/tool".into(),
                mode: 0o755,
                size_bytes: 4,
                sha256: digest(b"tool"),
            },
        ];
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
            root_mode: Some(0o755),
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

    fn pull_request<'a>(
        program: &'a str,
        host: &'a str,
        root: &'a str,
        transfer: &'a ArtifactTransferLease,
        resume: &'a ResumePlan,
    ) -> ReceiverPullRequest<'a> {
        ReceiverPullRequest {
            rsync_program: Path::new(program),
            remote_host: host,
            remote_store_root: Path::new(root),
            transfer,
            resume,
            timeout_seconds: 30,
        }
    }

    enum TestArchiveEntry<'a> {
        Directory(&'a str, u32),
        DirectoryPayload(&'a str, u32, &'a [u8]),
        PaxExtension(&'a [u8]),
        File(&'a str, u32, &'a [u8]),
        Symlink(&'a str, &'a str),
    }

    fn test_archive(entries: &[TestArchiveEntry<'_>]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for entry in entries {
            match entry {
                TestArchiveEntry::Directory(path, mode) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(*mode);
                    header.set_size(0);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, *path, std::io::empty())
                        .unwrap();
                }
                TestArchiveEntry::DirectoryPayload(path, mode, bytes) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(*mode);
                    header.set_size(bytes.len() as u64);
                    header.set_cksum();
                    builder.append_data(&mut header, *path, *bytes).unwrap();
                }
                TestArchiveEntry::PaxExtension(bytes) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::XHeader);
                    header.set_mode(0o644);
                    header.set_size(bytes.len() as u64);
                    header.set_cksum();
                    builder.append_data(&mut header, "pax", *bytes).unwrap();
                }
                TestArchiveEntry::File(path, mode, bytes) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_mode(*mode);
                    header.set_size(bytes.len() as u64);
                    header.set_cksum();
                    builder.append_data(&mut header, *path, *bytes).unwrap();
                }
                TestArchiveEntry::Symlink(path, target) => {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_mode(0o777);
                    header.set_size(0);
                    header.set_link_name(*target).unwrap();
                    header.set_cksum();
                    builder
                        .append_data(&mut header, *path, std::io::empty())
                        .unwrap();
                }
            }
        }
        builder.finish().unwrap();
        let raw = builder.into_inner().unwrap();
        zstd::stream::encode_all(raw.as_slice(), 1).unwrap()
    }

    fn archive_manifest(bytes: &[u8], entries: Vec<LayoutEntry>) -> ArtifactManifest {
        let mut manifest = fixture(bytes);
        manifest.layout_sha256 = digest(&serde_json::to_vec(&entries).unwrap());
        manifest.entries = entries;
        manifest
    }

    fn test_space_policy() -> SpacePolicy {
        SpacePolicy {
            minimum_free_bytes: 0,
        }
    }

    fn replace_first_tar_path(bytes: &[u8], path: &str) -> Vec<u8> {
        let mut raw = zstd::stream::decode_all(bytes).unwrap();
        assert!(path.len() < 100);
        raw[..100].fill(0);
        raw[..path.len()].copy_from_slice(path.as_bytes());
        raw[148..156].fill(b' ');
        let checksum: u32 = raw[..512].iter().map(|byte| u32::from(*byte)).sum();
        let encoded = format!("{checksum:06o}\0 ");
        raw[148..156].copy_from_slice(encoded.as_bytes());
        zstd::stream::encode_all(raw.as_slice(), 1).unwrap()
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
        let hostile_paths = [
            "../escape".into(),
            "/absolute".into(),
            "dir\\file".into(),
            "a/../b".into(),
            "C:/windows".into(),
            "a//b".into(),
            "space bad".into(),
            "a/".repeat(MAX_LAYOUT_PATH_DEPTH),
            "a".repeat(MAX_LAYOUT_COMPONENT_BYTES + 1),
            "a".repeat(MAX_LAYOUT_PATH_BYTES + 1),
        ];
        for hostile in hostile_paths {
            let mut manifest = fixture(&bytes);
            if let LayoutEntry::File { path, .. } = &mut manifest.entries[1] {
                *path = hostile.clone();
            }
            manifest.layout_sha256 = digest(&serde_json::to_vec(&manifest.entries).unwrap());
            assert!(manifest.validate().is_err(), "accepted {hostile}");
        }
        let mut manifest = fixture(&bytes);
        manifest.chunks[1].offset += 1;
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn rejects_paths_that_alias_on_case_insensitive_filesystems() {
        let bytes = vec![1; TEST_CHUNK_SIZE + 2];
        for paths in [["A", "a"], ["A/x", "a/y"]] {
            let entries = paths
                .into_iter()
                .map(|path| LayoutEntry::File {
                    path: path.into(),
                    mode: 0o644,
                    size_bytes: 1,
                    sha256: digest(b"a"),
                })
                .collect();
            let mut manifest = archive_manifest(&bytes, entries);
            manifest.schema = LEGACY_MANIFEST_SCHEMA;
            manifest.root_mode = None;
            assert!(
                matches!(manifest.validate(), Err(Error::Invalid(message)) if message.contains("case-insensitive"))
            );
        }

        for path in ["CON", "nul.txt", "COM1.log", "LPT9", "trailing."] {
            let manifest = archive_manifest(
                &bytes,
                vec![LayoutEntry::File {
                    path: path.into(),
                    mode: 0o644,
                    size_bytes: 1,
                    sha256: digest(b"a"),
                }],
            );
            assert!(manifest.validate().is_err(), "accepted {path}");
        }
        for path in ["conduit", "com10", "auxiliary.txt"] {
            assert!(validate_portable_component(path).is_ok(), "rejected {path}");
        }
    }

    #[test]
    fn manifest_schema_two_requires_parents_but_schema_one_remains_readable() {
        let tool = b"tool";
        let bytes = test_archive(&[TestArchiveEntry::File("bin/tool", 0o755, tool)]);
        let entries = vec![LayoutEntry::File {
            path: "bin/tool".into(),
            mode: 0o755,
            size_bytes: tool.len() as u64,
            sha256: digest(tool),
        }];
        let mut manifest = archive_manifest(&bytes, entries);
        assert!(manifest.validate().is_err());
        manifest.schema = LEGACY_MANIFEST_SCHEMA;
        manifest.root_mode = None;
        manifest.validate().unwrap();
        manifest.canonical_sha256().unwrap();
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("legacy.tar.zst");
        fs::write(&archive, bytes).unwrap();
        verify_archive_layout(&archive, &manifest).unwrap();
        let destination = temp.path().join("legacy");
        assert_eq!(
            extract_verified_archive(
                &archive,
                &manifest,
                &authority(&manifest),
                &destination,
                test_space_policy(),
            )
            .unwrap(),
            PublicationOutcome::Durable
        );
        assert_eq!(fs::read(destination.join("bin/tool")).unwrap(), tool);
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
        if let LayoutEntry::File { mode, .. } = &mut manifest.entries[1] {
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
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let transfer = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run")
            .unwrap();
        let partial = transfer.partial_path().to_path_buf();
        fs::write(&partial, &bytes[..TEST_CHUNK_SIZE + 17]).unwrap();
        let plan = plan_verified_resume(&transfer, &manifest).unwrap();
        assert_eq!(
            plan.disposition(),
            ResumeDisposition::Append {
                verified_bytes: MIN_CHUNK_SIZE
            }
        );
        let _prepared = apply_resume_plan(&transfer, &manifest, plan).unwrap();
        assert_eq!(fs::metadata(&partial).unwrap().len(), MIN_CHUNK_SIZE);

        let mut corrupt = bytes.clone();
        corrupt[TEST_CHUNK_SIZE + 1] ^= 1;
        fs::write(&partial, &corrupt).unwrap();
        assert_eq!(
            plan_verified_resume(&transfer, &manifest)
                .unwrap()
                .disposition(),
            ResumeDisposition::Append {
                verified_bytes: MIN_CHUNK_SIZE
            }
        );
    }

    #[test]
    fn fresh_transfer_plans_and_prepares_an_empty_restart_without_manual_files() {
        let bytes = vec![6; TEST_CHUNK_SIZE];
        let manifest = fixture(&bytes);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let transfer = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "fresh")
            .unwrap();
        assert!(!transfer.partial_path().exists());
        let plan = plan_verified_resume(&transfer, &manifest).unwrap();
        assert_eq!(plan.disposition(), ResumeDisposition::Restart);
        assert!(!transfer.partial_path().exists());
        let prepared = apply_resume_plan(&transfer, &manifest, plan).unwrap();
        assert_eq!(fs::metadata(transfer.partial_path()).unwrap().len(), 0);

        #[cfg(windows)]
        let absolute_rsync = r"C:\Shipyard\rsync.exe";
        #[cfg(not(windows))]
        let absolute_rsync = "/usr/bin/rsync";
        let command = receiver_pull_command(&pull_request(
            absolute_rsync,
            "m1-lan",
            "/var/lib/shipyard/artifacts",
            &transfer,
            &prepared,
        ))
        .unwrap();
        assert!(!command.args.iter().any(|arg| arg == "--append"));
    }

    #[test]
    fn prepared_prefix_and_openrsync_stats_bind_exact_payload_bytes() {
        let bytes = vec![6; TEST_CHUNK_SIZE * 2];
        let manifest = fixture(&bytes);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let transfer = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "resume-stats")
            .unwrap();
        fs::write(transfer.partial_path(), &bytes[..TEST_CHUNK_SIZE]).unwrap();
        let prepared = apply_resume_plan(
            &transfer,
            &manifest,
            plan_verified_resume(&transfer, &manifest).unwrap(),
        )
        .unwrap();
        let prefix = prepared_resume_evidence(&transfer, &manifest, &prepared).unwrap();
        assert_eq!(prefix.verified_prefix_bytes, MIN_CHUNK_SIZE);
        assert_eq!(
            prefix.verified_prefix_sha256,
            digest(&bytes[..TEST_CHUNK_SIZE])
        );

        let remaining = bytes.len() as u64 - MIN_CHUNK_SIZE;
        let stats = parse_rsync_transfer_stats(rsync_output(
            true,
            format!(
                "Number of files: 1\nTotal file size: {} bytes\nTotal transferred file size: {} bytes\nLiteral data: {} bytes\nMatched data: 0 bytes\nTotal bytes sent: 42\nTotal bytes received: {}\n",
                bytes.len(),
                bytes.len(),
                remaining,
                remaining + 73
            )
            .into_bytes(),
        ))
        .unwrap();
        let evidence = bind_receiver_pull_stats(prefix, stats).unwrap();
        assert_eq!(evidence.artifact_bytes_reused, MIN_CHUNK_SIZE);
        assert_eq!(evidence.artifact_bytes_transferred, remaining);
        assert_eq!(
            evidence
                .artifact_bytes_reused
                .checked_add(evidence.artifact_bytes_transferred),
            Some(evidence.artifact_bytes_total)
        );

        let fresh = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "full-stats")
            .unwrap();
        let fresh_prepared = apply_resume_plan(
            &fresh,
            &manifest,
            plan_verified_resume(&fresh, &manifest).unwrap(),
        )
        .unwrap();
        let fresh_prefix = prepared_resume_evidence(&fresh, &manifest, &fresh_prepared).unwrap();
        let full_stats = parse_rsync_transfer_stats(rsync_output(
            true,
            format!(
                "Total file size: {} bytes\nTotal transferred file size: {} bytes\nLiteral data: {} bytes\nMatched data: 0 bytes\nTotal bytes sent: 42\nTotal bytes received: {}\n",
                bytes.len(),
                bytes.len(),
                bytes.len(),
                bytes.len() + 73
            )
            .into_bytes(),
        ))
        .unwrap();
        let full = bind_receiver_pull_stats(fresh_prefix, full_stats).unwrap();
        assert_eq!(full.artifact_bytes_reused, 0);
        assert_eq!(full.artifact_bytes_transferred, bytes.len() as u64);
    }

    #[test]
    fn rsync_stats_parser_rejects_claims_without_complete_exact_counters() {
        let duplicate = b"Total file size: 10 bytes\nTotal file size: 10 bytes\nTotal transferred file size: 10 bytes\nLiteral data: 10 bytes\nMatched data: 0 bytes\nTotal bytes sent: 5\nTotal bytes received: 15\n";
        assert!(parse_rsync_transfer_stats(rsync_output(true, duplicate.to_vec())).is_err());
        let missing = b"Total file size: 10 bytes\nLiteral data: 10 bytes\n";
        assert!(parse_rsync_transfer_stats(rsync_output(true, missing.to_vec())).is_err());
        let complete = b"Total file size: 10 bytes\nTotal transferred file size: 10 bytes\nLiteral data: 10 bytes\nMatched data: 0 bytes\nTotal bytes sent: 5\nTotal bytes received: 15\n";
        assert!(parse_rsync_transfer_stats(rsync_output(false, complete.to_vec())).is_err());
        for malformed in ["1,0", "12,,345", ",10", "1234,567"] {
            let claims = format!(
                "Total file size: {malformed} bytes\nTotal transferred file size: 10 bytes\nLiteral data: 10 bytes\nMatched data: 0 bytes\nTotal bytes sent: 5\nTotal bytes received: 15\n"
            );
            assert!(parse_rsync_transfer_stats(rsync_output(true, claims.into_bytes())).is_err());
        }

        let prepared = PreparedResumeEvidence {
            artifact_sha256: "a".repeat(64),
            artifact_size_bytes: 10,
            manifest_sha256: "b".repeat(64),
            session: "stats".to_owned(),
            disposition: ResumeDisposition::Append { verified_bytes: 5 },
            verified_prefix_bytes: 5,
            verified_prefix_sha256: "c".repeat(64),
        };
        let mismatched = RsyncTransferStats {
            total_file_size_bytes: 10,
            total_transferred_file_size_bytes: 10,
            literal_data_bytes: 10,
            matched_data_bytes: 0,
            total_bytes_sent: 5,
            total_bytes_received: 15,
        };
        assert!(bind_receiver_pull_stats(prepared, mismatched).is_err());
    }

    #[test]
    fn resume_restarts_for_first_chunk_corruption_or_oversize() {
        let bytes = vec![4; TEST_CHUNK_SIZE];
        let manifest = fixture(&bytes);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let transfer = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run")
            .unwrap();
        let partial = transfer.partial_path().to_path_buf();
        let mut corrupt = bytes.clone();
        corrupt[0] ^= 1;
        fs::write(&partial, &corrupt).unwrap();
        assert_eq!(
            plan_verified_resume(&transfer, &manifest)
                .unwrap()
                .disposition(),
            ResumeDisposition::Restart
        );
        fs::write(&partial, vec![4; TEST_CHUNK_SIZE + 1]).unwrap();
        assert_eq!(
            plan_verified_resume(&transfer, &manifest)
                .unwrap()
                .disposition(),
            ResumeDisposition::Restart
        );
    }

    #[test]
    fn receiver_pull_argv_is_shell_free_and_append_is_fenced() {
        let bytes = vec![1; TEST_CHUNK_SIZE * 2];
        let manifest = fixture(&bytes);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let transfer = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run-1")
            .unwrap();
        fs::write(transfer.partial_path(), &bytes[..TEST_CHUNK_SIZE]).unwrap();
        let append = apply_resume_plan(
            &transfer,
            &manifest,
            plan_verified_resume(&transfer, &manifest).unwrap(),
        )
        .unwrap();
        #[cfg(windows)]
        let absolute_rsync = r"C:\Shipyard\rsync.exe";
        #[cfg(not(windows))]
        let absolute_rsync = "/usr/bin/rsync";
        let command = receiver_pull_command(&pull_request(
            absolute_rsync,
            "m1-lan",
            "/var/lib/shipyard/artifacts",
            &transfer,
            &append,
        ))
        .unwrap();
        assert_eq!(command.program, Path::new(absolute_rsync));
        assert!(command.args.iter().any(|arg| arg == "--append"));
        for hostile in ["-oProxyCommand=bad", "host;bad", "host name"] {
            assert!(
                receiver_pull_command(&pull_request(
                    absolute_rsync,
                    hostile,
                    "/safe",
                    &transfer,
                    &append,
                ))
                .is_err()
            );
        }
        assert!(
            receiver_pull_command(&pull_request("rsync", "m1", "/safe", &transfer, &append,))
                .is_err()
        );
        assert!(
            receiver_pull_command(&pull_request(
                absolute_rsync,
                "m1",
                "/safe;bad",
                &transfer,
                &append,
            ))
            .is_err()
        );
        assert!(
            store
                .acquire_transfer_lease(&manifest.artifact_sha256, "../escape")
                .is_err()
        );
    }

    #[test]
    fn resume_plan_rejects_cross_session_replay_and_file_drift() {
        let bytes = vec![2; TEST_CHUNK_SIZE * 2];
        let manifest = fixture(&bytes);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let first = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "first")
            .unwrap();
        let second = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "second")
            .unwrap();
        fs::write(first.partial_path(), &bytes[..TEST_CHUNK_SIZE]).unwrap();
        fs::write(second.partial_path(), &bytes[..TEST_CHUNK_SIZE]).unwrap();
        let cross_session = plan_verified_resume(&first, &manifest).unwrap();
        assert!(apply_resume_plan(&second, &manifest, cross_session).is_err());

        let stale = plan_verified_resume(&first, &manifest).unwrap();
        OpenOptions::new()
            .append(true)
            .open(first.partial_path())
            .unwrap()
            .write_all(b"changed")
            .unwrap();
        assert!(apply_resume_plan(&first, &manifest, stale).is_err());

        fs::write(first.partial_path(), &bytes[..TEST_CHUNK_SIZE]).unwrap();
        let wrong_manifest_plan = plan_verified_resume(&first, &manifest).unwrap();
        let mut different_manifest = manifest.clone();
        different_manifest.producer.attempt += 1;
        assert!(apply_resume_plan(&first, &different_manifest, wrong_manifest_plan).is_err());

        fs::write(first.partial_path(), &bytes[..TEST_CHUNK_SIZE]).unwrap();
        let unprepared = plan_verified_resume(&first, &manifest).unwrap();
        assert!(
            receiver_pull_command(&pull_request(
                "/usr/bin/rsync",
                "m1",
                "/safe",
                &first,
                &unprepared,
            ))
            .is_err()
        );
        let prepared = apply_resume_plan(
            &first,
            &manifest,
            plan_verified_resume(&first, &manifest).unwrap(),
        )
        .unwrap();
        let mut changed = OpenOptions::new()
            .write(true)
            .open(first.partial_path())
            .unwrap();
        changed.write_all(b"X").unwrap();
        assert!(
            receiver_pull_command(&pull_request(
                "/usr/bin/rsync",
                "m1",
                "/safe",
                &first,
                &prepared,
            ))
            .is_err()
        );
    }

    #[test]
    fn archive_layout_verifies_and_extracts_only_the_exact_manifest() {
        let tool = b"verified tool";
        let bytes = test_archive(&[
            TestArchiveEntry::Directory("bin", 0o755),
            TestArchiveEntry::File("bin/tool", 0o755, tool),
        ]);
        let manifest = archive_manifest(
            &bytes,
            vec![
                LayoutEntry::Directory {
                    path: "bin".into(),
                    mode: 0o755,
                },
                LayoutEntry::File {
                    path: "bin/tool".into(),
                    mode: 0o755,
                    size_bytes: tool.len() as u64,
                    sha256: digest(tool),
                },
            ],
        );
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("artifact.tar.zst");
        fs::write(&archive, &bytes).unwrap();
        verify_archive_layout(&archive, &manifest).unwrap();
        let destination = temp.path().join("unpacked");
        let mut stale_authority = authority(&manifest);
        stale_authority.attempt += 1;
        assert!(matches!(
            extract_verified_archive(
                &archive,
                &manifest,
                &stale_authority,
                &destination,
                test_space_policy(),
            ),
            Err(Error::StaleFence(_))
        ));
        assert!(!destination.exists());
        let competing_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(temp.path().join(".shipyard-artifact-extract.lease"))
            .unwrap();
        FileExt::try_lock_exclusive(&competing_lock).unwrap();
        assert!(
            extract_verified_archive(
                &archive,
                &manifest,
                &authority(&manifest),
                &destination,
                test_space_policy(),
            )
            .is_err()
        );
        assert!(!destination.exists());
        FileExt::unlock(&competing_lock).unwrap();
        assert_eq!(
            extract_verified_archive(
                &archive,
                &manifest,
                &authority(&manifest),
                &destination,
                test_space_policy(),
            )
            .unwrap(),
            PublicationOutcome::Durable
        );
        assert_eq!(fs::read(destination.join("bin/tool")).unwrap(), tool);
        assert!(
            extract_verified_archive(
                &archive,
                &manifest,
                &authority(&manifest),
                &destination,
                test_space_policy(),
            )
            .is_err()
        );
    }

    #[test]
    fn extraction_rejects_directory_payloads_and_insufficient_space() {
        let payload = b"hidden payload";
        let bytes = test_archive(&[TestArchiveEntry::DirectoryPayload("bin", 0o755, payload)]);
        let manifest = archive_manifest(
            &bytes,
            vec![LayoutEntry::Directory {
                path: "bin".into(),
                mode: 0o755,
            }],
        );
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("directory-payload.tar.zst");
        fs::write(&archive, &bytes).unwrap();
        assert!(verify_archive_layout(&archive, &manifest).is_err());

        let tool = b"verified tool";
        let bytes = test_archive(&[TestArchiveEntry::File("tool", 0o755, tool)]);
        let manifest = archive_manifest(
            &bytes,
            vec![LayoutEntry::File {
                path: "tool".into(),
                mode: 0o755,
                size_bytes: tool.len() as u64,
                sha256: digest(tool),
            }],
        );
        let archive = temp.path().join("space.tar.zst");
        fs::write(&archive, &bytes).unwrap();
        let destination = temp.path().join("space-failed");
        let allocation_budget = manifest.unpacked_allocation_budget_bytes().unwrap();
        let unavailable = SpacePolicy {
            // Make the required total exactly u64::MAX.  A live free-space
            // reading can increase between this test's setup and extraction
            // (notably on Windows CI), so deriving the watermark from an
            // earlier reading makes this negative control nondeterministic.
            minimum_free_bytes: u64::MAX - allocation_budget,
        };
        assert!(matches!(
            extract_verified_archive(
                &archive,
                &manifest,
                &authority(&manifest),
                &destination,
                unavailable,
            ),
            Err(Error::InsufficientSpace { .. })
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn archive_rejects_raw_extension_records_before_their_payload_is_read() {
        let extension = b"17 path=hidden\n";
        let bytes = test_archive(&[
            TestArchiveEntry::PaxExtension(extension),
            TestArchiveEntry::File("safe", 0o644, b"x"),
        ]);
        let manifest = archive_manifest(
            &bytes,
            vec![LayoutEntry::File {
                path: "safe".into(),
                mode: 0o644,
                size_bytes: 1,
                sha256: digest(b"x"),
            }],
        );
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("extension.tar.zst");
        fs::write(&archive, bytes).unwrap();
        assert!(verify_archive_layout(&archive, &manifest).is_err());
    }

    #[test]
    fn staging_publication_is_no_replace_and_restores_cleanup_permissions() {
        let temp = TempDir::new().unwrap();
        let staging = tempfile::Builder::new()
            .prefix("staging-")
            .tempdir_in(temp.path())
            .unwrap();
        let staging_path = staging.path().to_path_buf();
        let restricted = staging.path().join("restricted");
        fs::create_dir(&restricted).unwrap();
        fs::write(restricted.join("payload"), b"payload").unwrap();
        let destination = temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("sentinel"), b"keep").unwrap();

        assert!(
            publish_staging_no_replace(staging, &destination, vec![(restricted, 0o000)]).is_err()
        );
        assert_eq!(fs::read(destination.join("sentinel")).unwrap(), b"keep");
        assert!(!staging_path.exists());

        let staging = tempfile::Builder::new()
            .prefix("staging-empty-race-")
            .tempdir_in(temp.path())
            .unwrap();
        let staging_path = staging.path().to_path_buf();
        fs::write(staging.path().join("payload"), b"payload").unwrap();
        let empty_destination = temp.path().join("empty-destination");
        fs::create_dir(&empty_destination).unwrap();
        assert!(publish_staging_no_replace(staging, &empty_destination, vec![]).is_err());
        assert!(empty_destination.read_dir().unwrap().next().is_none());
        assert!(!staging_path.exists());
    }

    #[test]
    fn publication_reports_visible_but_unsynced_destination_distinctly() {
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("published");
        let outcome = publication_outcome(&destination, temp.path(), |_| {
            Err(Error::Io(std::io::Error::other("injected sync failure")))
        });
        assert!(matches!(
            outcome,
            PublicationOutcome::PublishedParentSyncPending { destination: reported, message }
                if reported == destination && message.contains("injected sync failure")
        ));
    }

    #[test]
    fn allocation_budget_reserves_space_for_files_and_directories() {
        let bytes = vec![1; TEST_CHUNK_SIZE + 2];
        let manifest = archive_manifest(
            &bytes,
            vec![
                LayoutEntry::Directory {
                    path: "bin".into(),
                    mode: 0o755,
                },
                LayoutEntry::File {
                    path: "bin/tool".into(),
                    mode: 0o755,
                    size_bytes: 4,
                    sha256: digest(b"tool"),
                },
            ],
        );
        assert_eq!(
            manifest.unpacked_allocation_budget_bytes().unwrap(),
            4 + 2 * ENTRY_ALLOCATION_RESERVE_BYTES
        );
    }

    #[test]
    fn directory_creation_rechecks_the_live_space_watermark() {
        let temp = TempDir::new().unwrap();
        let mut observations = [
            2 * ENTRY_ALLOCATION_RESERVE_BYTES,
            ENTRY_ALLOCATION_RESERVE_BYTES - 1,
        ]
        .into_iter();
        let mut probe = |_path: &Path| {
            observations
                .next()
                .ok_or_else(|| Error::Invalid("unexpected extra space probe".into()))
        };
        let mut extraction = ExtractionContext {
            root: temp.path(),
            space_policy: test_space_policy(),
            remaining_bytes: 2 * ENTRY_ALLOCATION_RESERVE_BYTES,
            space_probe: &mut probe,
        };
        assert!(matches!(
            extraction.ensure_directories(Path::new("one/two")),
            Err(Error::InsufficientSpace { .. })
        ));
        assert!(temp.path().join("one").is_dir());
        assert!(!temp.path().join("one/two").exists());
    }

    #[test]
    fn archive_rejects_traversal_links_duplicates_and_undeclared_paths() {
        let file = b"x";
        let regular = test_archive(&[TestArchiveEntry::File("safe", 0o644, file)]);
        let entries = vec![LayoutEntry::File {
            path: "safe".into(),
            mode: 0o644,
            size_bytes: 1,
            sha256: digest(file),
        }];
        let temp = TempDir::new().unwrap();

        let traversal = replace_first_tar_path(&regular, "../escape");
        let traversal_manifest = archive_manifest(&traversal, entries.clone());
        let traversal_path = temp.path().join("traversal.tar.zst");
        fs::write(&traversal_path, traversal).unwrap();
        assert!(verify_archive_layout(&traversal_path, &traversal_manifest).is_err());

        let links = test_archive(&[TestArchiveEntry::Symlink("safe", "../escape")]);
        let link_manifest = archive_manifest(&links, entries.clone());
        let link_path = temp.path().join("link.tar.zst");
        fs::write(&link_path, links).unwrap();
        assert!(verify_archive_layout(&link_path, &link_manifest).is_err());

        let duplicates = test_archive(&[
            TestArchiveEntry::File("safe", 0o644, file),
            TestArchiveEntry::File("safe", 0o644, file),
        ]);
        let duplicate_manifest = archive_manifest(&duplicates, entries.clone());
        let duplicate_path = temp.path().join("duplicate.tar.zst");
        fs::write(&duplicate_path, duplicates).unwrap();
        assert!(verify_archive_layout(&duplicate_path, &duplicate_manifest).is_err());

        let extra = test_archive(&[
            TestArchiveEntry::File("safe", 0o644, file),
            TestArchiveEntry::File("extra", 0o644, file),
        ]);
        let extra_manifest = archive_manifest(&extra, entries);
        let extra_path = temp.path().join("extra.tar.zst");
        fs::write(&extra_path, extra).unwrap();
        assert!(verify_archive_layout(&extra_path, &extra_manifest).is_err());

        let elevated = test_archive(&[TestArchiveEntry::File("safe", 0o4755, file)]);
        let elevated_manifest = archive_manifest(
            &elevated,
            vec![LayoutEntry::File {
                path: "safe".into(),
                mode: 0o755,
                size_bytes: 1,
                sha256: digest(file),
            }],
        );
        let elevated_path = temp.path().join("elevated.tar.zst");
        fs::write(&elevated_path, elevated).unwrap();
        assert!(verify_archive_layout(&elevated_path, &elevated_manifest).is_err());

        let mut trailing_raw = zstd::stream::decode_all(regular.as_slice()).unwrap();
        trailing_raw.extend_from_slice(b"hidden-after-end");
        let trailing = zstd::stream::encode_all(trailing_raw.as_slice(), 1).unwrap();
        let trailing_manifest = archive_manifest(
            &trailing,
            vec![LayoutEntry::File {
                path: "safe".into(),
                mode: 0o644,
                size_bytes: 1,
                sha256: digest(file),
            }],
        );
        let trailing_path = temp.path().join("trailing.tar.zst");
        fs::write(&trailing_path, trailing).unwrap();
        assert!(verify_archive_layout(&trailing_path, &trailing_manifest).is_err());
    }

    #[test]
    fn archive_trailing_padding_accepts_standard_tar_records_but_stays_bounded() {
        reject_trailing_archive_data(std::io::Cursor::new(vec![0_u8; MAX_TAR_ZERO_PADDING_BYTES]))
            .unwrap();
        assert!(
            reject_trailing_archive_data(std::io::Cursor::new(vec![
                0_u8;
                MAX_TAR_ZERO_PADDING_BYTES + 1
            ]))
            .is_err()
        );
        let mut nonzero = vec![0_u8; 512];
        nonzero[511] = 1;
        assert!(reject_trailing_archive_data(std::io::Cursor::new(nonzero)).is_err());
    }

    #[test]
    fn archive_rejects_partial_layout_and_type_size_mode_or_digest_mismatch() {
        let file = b"payload";
        let bytes = test_archive(&[TestArchiveEntry::File("tool", 0o755, file)]);
        let base_entry = LayoutEntry::File {
            path: "tool".into(),
            mode: 0o755,
            size_bytes: file.len() as u64,
            sha256: digest(file),
        };
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("artifact.tar.zst");
        fs::write(&archive, &bytes).unwrap();

        let mut partial_entries = vec![base_entry.clone()];
        partial_entries.push(LayoutEntry::File {
            path: "missing".into(),
            mode: 0o644,
            size_bytes: 1,
            sha256: digest(b"x"),
        });
        partial_entries.sort_by(|left, right| left.path().cmp(right.path()));
        let partial = archive_manifest(&bytes, partial_entries);
        assert!(verify_archive_layout(&archive, &partial).is_err());

        for hostile in [
            LayoutEntry::Directory {
                path: "tool".into(),
                mode: 0o755,
            },
            LayoutEntry::File {
                path: "tool".into(),
                mode: 0o644,
                size_bytes: file.len() as u64,
                sha256: digest(file),
            },
            LayoutEntry::File {
                path: "tool".into(),
                mode: 0o755,
                size_bytes: file.len() as u64 + 1,
                sha256: digest(file),
            },
            LayoutEntry::File {
                path: "tool".into(),
                mode: 0o755,
                size_bytes: file.len() as u64,
                sha256: "0".repeat(64),
            },
        ] {
            let manifest = archive_manifest(&bytes, vec![hostile]);
            assert!(verify_archive_layout(&archive, &manifest).is_err());
            let destination = temp
                .path()
                .join(format!("failed-{}", manifest.layout_sha256));
            assert!(
                extract_verified_archive(
                    &archive,
                    &manifest,
                    &authority(&manifest),
                    &destination,
                    test_space_policy(),
                )
                .is_err()
            );
            assert!(!destination.exists());
        }
    }

    #[test]
    fn transfer_lease_is_exclusive_and_recovers_after_owner_exit() {
        let digest = "a".repeat(64);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let first = store.acquire_transfer_lease(&digest, "run-1").unwrap();
        assert_eq!(first.artifact_sha256(), digest);
        assert_eq!(first.session(), "run-1");
        assert!(store.acquire_transfer_lease(&digest, "run-1").is_err());
        drop(first);
        store.acquire_transfer_lease(&digest, "run-1").unwrap();
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
        let first = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run-1")
            .unwrap();
        let partial = first.partial_path().to_path_buf();
        fs::write(&partial, &bytes).unwrap();
        let outcome = store.publish_verified(&manifest, &auth, first).unwrap();
        let PublishOutcome::Published(published) = outcome else {
            panic!("expected publication")
        };
        assert!(!partial.exists());
        assert_eq!(fs::read(&published).unwrap(), bytes);

        let second_lease = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run-2")
            .unwrap();
        let second = second_lease.partial_path().to_path_buf();
        fs::write(&second, &bytes).unwrap();
        assert!(matches!(
            store
                .publish_verified(&manifest, &auth, second_lease)
                .unwrap(),
            PublishOutcome::Reused(_)
        ));

        let bad_lease = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run-3")
            .unwrap();
        let bad = bad_lease.partial_path().to_path_buf();
        let sealed = bad_lease.sealed_path.clone();
        let mut corrupt = bytes.clone();
        corrupt[0] ^= 1;
        fs::write(&bad, corrupt).unwrap();
        assert!(store.publish_verified(&manifest, &auth, bad_lease).is_err());
        assert!(bad.exists());
        assert!(!sealed.exists());
    }

    #[test]
    fn concurrent_publishers_never_clobber_and_publish_the_named_digest() {
        use std::sync::{Arc, Barrier};

        let bytes = vec![6; TEST_CHUNK_SIZE * 2 + 17];
        let manifest = fixture(&bytes);
        let auth = authority(&manifest);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let first = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run-a")
            .unwrap();
        let second = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run-b")
            .unwrap();
        fs::write(first.partial_path(), &bytes).unwrap();
        fs::write(second.partial_path(), &bytes).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let publish = |store: ArtifactStore,
                       manifest: ArtifactManifest,
                       auth: ManifestAuthority,
                       transfer: ArtifactTransferLease,
                       barrier: Arc<Barrier>| {
            std::thread::spawn(move || {
                store.publish_verified_with_hook(&manifest, &auth, transfer, |_| {
                    barrier.wait();
                    Ok(())
                })
            })
        };
        let a = publish(
            store.clone(),
            manifest.clone(),
            auth.clone(),
            first,
            barrier.clone(),
        );
        let b = publish(store.clone(), manifest.clone(), auth, second, barrier);
        let outcomes = [a.join().unwrap().unwrap(), b.join().unwrap().unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, PublishOutcome::Published(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, PublishOutcome::Reused(_)))
                .count(),
            1
        );
        let published = store
            .root
            .join("objects")
            .join(format!("{}.tar.zst", manifest.artifact_sha256));
        let mut file = File::open(published).unwrap();
        assert_eq!(sha256_reader(&mut file).unwrap(), manifest.artifact_sha256);
    }

    #[test]
    fn destination_created_at_publication_is_verified_and_never_overwritten() {
        let bytes = vec![7; TEST_CHUNK_SIZE + 29];
        let manifest = fixture(&bytes);
        let auth = authority(&manifest);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let transfer = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run-race")
            .unwrap();
        let sealed = transfer.sealed_path.clone();
        fs::write(transfer.partial_path(), &bytes).unwrap();
        let hostile = vec![0; bytes.len()];
        let hostile_copy = hostile.clone();
        let result =
            store.publish_verified_with_hook(&manifest, &auth, transfer, move |destination| {
                fs::write(destination, hostile_copy)?;
                Ok(())
            });
        assert!(matches!(result, Err(Error::Invalid(_))));
        let destination = store
            .root
            .join("objects")
            .join(format!("{}.tar.zst", manifest.artifact_sha256));
        assert_eq!(fs::read(destination).unwrap(), hostile);
        assert!(sealed.exists(), "verified source must survive the conflict");
    }

    #[test]
    fn sealed_transfer_survives_restart_and_cannot_be_received_again() {
        let bytes = vec![8; TEST_CHUNK_SIZE + 41];
        let manifest = fixture(&bytes);
        let auth = authority(&manifest);
        let temp = TempDir::new().unwrap();
        let store = ArtifactStore::open(temp.path().join("store")).unwrap();
        let transfer = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run-restart")
            .unwrap();
        let sealed = transfer.sealed_path.clone();
        fs::write(transfer.partial_path(), &bytes).unwrap();
        assert!(
            store
                .publish_verified_with_hook(&manifest, &auth, transfer, |_| {
                    Err(Error::Invalid("simulated process loss".into()))
                })
                .is_err()
        );
        assert!(sealed.exists());

        let resumed = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run-restart")
            .unwrap();
        assert!(plan_verified_resume(&resumed, &manifest).is_err());
        assert!(matches!(
            store.publish_verified(&manifest, &auth, resumed).unwrap(),
            PublishOutcome::Published(_)
        ));
        assert!(!sealed.exists());
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

    #[test]
    fn schema_two_manifest_without_root_mode_remains_readable() {
        let mut value = serde_json::to_value(fixture(&vec![8; TEST_CHUNK_SIZE])).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("schema".into(), serde_json::json!(PREVIOUS_MANIFEST_SCHEMA));
        object.remove("root_mode");
        let manifest: ArtifactManifest = serde_json::from_value(value).unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.root_mode, None);
    }

    fn build_tree_inputs() -> BuildTreeArtifactInputs {
        let template = fixture(&vec![1; TEST_CHUNK_SIZE]);
        BuildTreeArtifactInputs {
            source: template.source,
            build: template.build,
            cache_generations: template.cache_generations,
            producer: template.producer,
        }
    }

    #[cfg(unix)]
    fn packed_build_tree(temp: &TempDir) -> (PathBuf, ArtifactManifest) {
        let source = temp.path().join("configured-build");
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("CMakeCache.txt"), b"configured").unwrap();
        fs::write(source.join("bin/test-runner"), b"executable").unwrap();
        #[cfg(unix)]
        {
            set_portable_permissions(&source, 0o750).unwrap();
            set_portable_permissions(&source.join("bin/test-runner"), 0o755).unwrap();
        }
        let archive = temp.path().join("configured-build.tar.zst");
        let outcome = pack_verified_build_tree(&source, &archive, build_tree_inputs()).unwrap();
        assert_eq!(outcome.publication, PublicationOutcome::Durable);
        (archive, outcome.manifest)
    }

    fn restorable_build_tree(temp: &TempDir) -> (PathBuf, ArtifactManifest) {
        let cache = b"configured";
        let runner = b"executable";
        let bytes = test_archive(&[
            TestArchiveEntry::File("CMakeCache.txt", 0o644, cache),
            TestArchiveEntry::Directory("bin", 0o755),
            TestArchiveEntry::File("bin/test-runner", 0o755, runner),
        ]);
        let entries = vec![
            LayoutEntry::File {
                path: "CMakeCache.txt".into(),
                mode: 0o644,
                size_bytes: cache.len() as u64,
                sha256: digest(cache),
            },
            LayoutEntry::Directory {
                path: "bin".into(),
                mode: 0o755,
            },
            LayoutEntry::File {
                path: "bin/test-runner".into(),
                mode: 0o755,
                size_bytes: runner.len() as u64,
                sha256: digest(runner),
            },
        ];
        let mut manifest = archive_manifest(&bytes, entries);
        manifest.root_mode = Some(0o750);
        let archive = temp.path().join("configured-build.tar.zst");
        fs::write(&archive, bytes).unwrap();
        manifest.validate().unwrap();
        (archive, manifest)
    }

    #[cfg(unix)]
    #[test]
    fn build_tree_pack_round_trips_exact_layout_and_modes() {
        let temp = TempDir::new().unwrap();
        let (archive, manifest) = packed_build_tree(&temp);
        verify_archive_layout(&archive, &manifest).unwrap();
        let destination = temp.path().join("restored");
        assert_eq!(
            extract_verified_archive(
                &archive,
                &manifest,
                &authority(&manifest),
                &destination,
                test_space_policy(),
            )
            .unwrap(),
            PublicationOutcome::Durable
        );
        assert_eq!(
            fs::read(destination.join("CMakeCache.txt")).unwrap(),
            b"configured"
        );
        assert_eq!(
            fs::read(destination.join("bin/test-runner")).unwrap(),
            b"executable"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(destination.join("bin/test-runner"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                0o750
            );
        }
    }

    #[test]
    fn build_tree_pack_is_no_replace() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("object.o"), b"object").unwrap();
        let archive = temp.path().join("artifact.tar.zst");
        fs::write(&archive, b"sentinel").unwrap();
        assert!(pack_verified_build_tree(&source, &archive, build_tree_inputs()).is_err());
        assert_eq!(fs::read(archive).unwrap(), b"sentinel");
    }

    #[cfg(not(unix))]
    #[test]
    fn build_tree_pack_fails_closed_without_no_follow_directory_handles() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("object.o"), b"object").unwrap();
        let archive = temp.path().join("artifact.tar.zst");

        let result = pack_verified_build_tree(&source, &archive, build_tree_inputs());

        assert!(matches!(
            result,
            Err(Error::Invalid(message))
                if message.contains("requires no-follow directory handles")
        ));
        assert!(!archive.exists());
    }

    #[cfg(unix)]
    #[test]
    fn build_tree_pack_supports_ustar_paths_and_rejects_extension_records() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let first = "a".repeat(70);
        let second = "b".repeat(70);
        let long_relative = Path::new(&first).join(&second).join("object.o");
        fs::create_dir_all(source.join(&first).join(&second)).unwrap();
        fs::write(source.join(&long_relative), b"object").unwrap();
        let archive = temp.path().join("ustar.tar.zst");
        let outcome = pack_verified_build_tree(&source, &archive, build_tree_inputs()).unwrap();
        verify_archive_layout(&archive, &outcome.manifest).unwrap();
        assert!(
            outcome
                .manifest
                .entries
                .iter()
                .any(|entry| entry.path() == long_relative.to_str().unwrap())
        );

        let third = "c".repeat(70);
        let fourth = "d".repeat(70);
        let unsupported = source.join(&first).join(&second).join(&third).join(&fourth);
        fs::create_dir_all(&unsupported).unwrap();
        fs::write(unsupported.join("object.o"), b"object").unwrap();
        let rejected = temp.path().join("longlink.tar.zst");
        assert!(pack_verified_build_tree(&source, &rejected, build_tree_inputs()).is_err());
        assert!(!rejected.exists());
    }

    #[test]
    fn build_tree_restore_quarantines_and_replaces_existing_tree() {
        let temp = TempDir::new().unwrap();
        let (archive, manifest) = restorable_build_tree(&temp);
        let destination = temp.path().join("active-build");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("stale"), b"old").unwrap();
        let outcome = restore_verified_build_tree(
            &archive,
            &manifest,
            &authority(&manifest),
            &destination,
            test_space_policy(),
        )
        .unwrap();
        assert_eq!(outcome.publication, PublicationOutcome::Durable);
        assert!(outcome.replaced_existing);
        let quarantine = outcome.quarantine_cleanup_pending.unwrap();
        assert_eq!(fs::read(quarantine.join("prior/stale")).unwrap(), b"old");
        assert!(!destination.join("stale").exists());
        assert_eq!(
            fs::read(destination.join("CMakeCache.txt")).unwrap(),
            b"configured"
        );
        fs::remove_dir_all(quarantine).unwrap();
    }

    #[test]
    fn build_tree_restore_rolls_back_after_extraction_failure() {
        let temp = TempDir::new().unwrap();
        let (archive, manifest) = restorable_build_tree(&temp);
        let destination = temp.path().join("active-build");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("sentinel"), b"old").unwrap();
        let result = restore_verified_build_tree_with(
            &archive,
            &manifest,
            &authority(&manifest),
            &destination,
            test_space_policy(),
            |_, _, _, _, _| Err(Error::Invalid("injected extraction failure".into())),
        );
        assert!(
            matches!(result, Err(Error::Invalid(message)) if message == "injected extraction failure")
        );
        assert_eq!(fs::read(destination.join("sentinel")).unwrap(), b"old");
        assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".shipyard-build-tree-quarantine-")
        }));
    }

    #[test]
    fn rollback_atomically_refuses_a_destination_recreated_after_preflight() {
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("active-build");
        let quarantine = temp.path().join("quarantine");
        let prior = quarantine.join("prior");
        fs::create_dir_all(&prior).unwrap();
        fs::write(prior.join("sentinel"), b"old").unwrap();
        let original = Error::Invalid("injected extraction failure".into());

        let result = rollback_quarantined_tree_with(
            &destination,
            temp.path(),
            &quarantine,
            &prior,
            &original,
            |source, target| {
                fs::create_dir(target)?;
                fs::write(target.join("contender"), b"new")?;
                rename_no_replace(source, target)
            },
        );

        assert!(
            matches!(result, Err(Error::Invalid(message)) if message.contains("rollback failed"))
        );
        assert_eq!(fs::read(destination.join("contender")).unwrap(), b"new");
        assert_eq!(fs::read(prior.join("sentinel")).unwrap(), b"old");
    }

    #[test]
    fn build_tree_restore_rejects_tamper_before_quarantine() {
        let temp = TempDir::new().unwrap();
        let (archive, manifest) = restorable_build_tree(&temp);
        let destination = temp.path().join("active-build");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("sentinel"), b"old").unwrap();
        let mut archive_bytes = fs::read(&archive).unwrap();
        archive_bytes[0] ^= 1;
        fs::write(&archive, archive_bytes).unwrap();
        assert!(
            restore_verified_build_tree(
                &archive,
                &manifest,
                &authority(&manifest),
                &destination,
                test_space_policy(),
            )
            .is_err()
        );
        assert_eq!(fs::read(destination.join("sentinel")).unwrap(), b"old");

        let tampered_manifest_temp = TempDir::new().unwrap();
        let (archive, mut manifest) = restorable_build_tree(&tampered_manifest_temp);
        let LayoutEntry::File { sha256, .. } = manifest
            .entries
            .iter_mut()
            .find(|entry| matches!(entry, LayoutEntry::File { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        *sha256 = "0".repeat(64);
        manifest.layout_sha256 = digest(&serde_json::to_vec(&manifest.entries).unwrap());
        assert!(
            restore_verified_build_tree(
                &archive,
                &manifest,
                &authority(&manifest),
                &destination,
                test_space_policy(),
            )
            .is_err()
        );
        assert_eq!(fs::read(destination.join("sentinel")).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn build_tree_pack_and_restore_reject_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("target"), b"target").unwrap();
        symlink(source.join("target"), source.join("link")).unwrap();
        assert!(
            pack_verified_build_tree(
                &source,
                &temp.path().join("rejected.tar.zst"),
                build_tree_inputs(),
            )
            .is_err()
        );

        fs::remove_file(source.join("link")).unwrap();
        let archive = temp.path().join("accepted.tar.zst");
        let manifest = pack_verified_build_tree(&source, &archive, build_tree_inputs())
            .unwrap()
            .manifest;
        let destination_target = temp.path().join("destination-target");
        fs::create_dir(&destination_target).unwrap();
        let destination = temp.path().join("destination-link");
        symlink(&destination_target, &destination).unwrap();
        assert!(
            restore_verified_build_tree(
                &archive,
                &manifest,
                &authority(&manifest),
                &destination,
                test_space_policy(),
            )
            .is_err()
        );
        assert!(fs::read_dir(destination_target).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn build_tree_pack_rejects_hard_link_topology() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("first"), b"shared").unwrap();
        fs::hard_link(source.join("first"), source.join("second")).unwrap();

        let result = pack_verified_build_tree(
            &source,
            &temp.path().join("rejected.tar.zst"),
            build_tree_inputs(),
        );

        assert!(matches!(result, Err(Error::Invalid(message)) if message.contains("hard-linked")));
    }

    #[cfg(unix)]
    #[test]
    fn build_tree_pack_reads_only_from_the_pinned_root() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("payload"), b"authorized").unwrap();
        let observed = observe_build_tree(&source).unwrap();

        let displaced = temp.path().join("displaced");
        fs::rename(&source, &displaced).unwrap();
        let external = temp.path().join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("payload"), b"secret").unwrap();
        symlink(&external, &source).unwrap();

        let archive_path = temp.path().join("pinned.tar.zst");
        let archive = File::create(&archive_path).unwrap();
        write_build_tree_archive(&observed, &archive).unwrap();
        let decoded = zstd::stream::decode_all(File::open(archive_path).unwrap()).unwrap();
        let mut tar = tar::Archive::new(decoded.as_slice());
        let mut entry = tar.entries().unwrap().next().unwrap().unwrap();
        let mut payload = Vec::new();
        entry.read_to_end(&mut payload).unwrap();
        assert_eq!(payload, b"authorized");
        assert_ne!(
            same_file::Handle::from_path(&source).unwrap(),
            observed.root_identity
        );
        fs::remove_file(&source).unwrap();
        symlink(&displaced, &source).unwrap();
        assert!(validate_observed_build_tree(&source, &observed, &observed.entries).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn build_tree_pack_detects_root_mode_mutation() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("payload"), b"authorized").unwrap();
        set_portable_permissions(&source, 0o755).unwrap();
        let observed = observe_build_tree(&source).unwrap();
        let expected = observed.entries.clone();
        set_portable_permissions(&source, 0o700).unwrap();
        assert!(validate_observed_build_tree(&source, &observed, &expected).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_child_open_rejects_special_file_without_blocking() {
        use rustix::fs::{Mode, open};
        let temp = TempDir::new().unwrap();
        let socket = temp.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let parent = File::from(
            open(
                temp.path(),
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap(),
        );
        assert!(open_pinned_child(&parent, std::ffi::OsStr::new("socket"), false).is_err());
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
        let transfer = store
            .acquire_transfer_lease(&manifest.artifact_sha256, "run")
            .unwrap();
        let external = temp.path().join("external");
        fs::write(&external, &bytes).unwrap();
        symlink(&external, transfer.partial_path()).unwrap();
        assert!(
            store
                .publish_verified(&manifest, &authority(&manifest), transfer)
                .is_err()
        );
    }

    #[test]
    fn build_tree_restore_preserves_quarantine_when_destination_reappears() {
        let temp = TempDir::new().unwrap();
        let (archive, manifest) = restorable_build_tree(&temp);
        let destination = temp.path().join("active-build");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("sentinel"), b"old").unwrap();
        let result = restore_verified_build_tree_with(
            &archive,
            &manifest,
            &authority(&manifest),
            &destination,
            test_space_policy(),
            |_, _, _, destination, _| {
                fs::create_dir(destination)?;
                Err(Error::Invalid("injected extraction failure".into()))
            },
        );
        assert!(
            matches!(result, Err(Error::Invalid(message)) if message.contains("destination reappeared"))
        );
        let quarantine = fs::read_dir(temp.path())
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".shipyard-build-tree-quarantine-")
            })
            .unwrap()
            .path();
        assert_eq!(fs::read(quarantine.join("prior/sentinel")).unwrap(), b"old");
        fs::remove_dir(&destination).unwrap();
        fs::remove_dir_all(quarantine).unwrap();
    }

    #[test]
    fn build_tree_restore_retains_prior_tree_until_publication_is_durable() {
        let temp = TempDir::new().unwrap();
        let (archive, manifest) = restorable_build_tree(&temp);
        let destination = temp.path().join("active-build");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("sentinel"), b"old").unwrap();
        let outcome = restore_verified_build_tree_with(
            &archive,
            &manifest,
            &authority(&manifest),
            &destination,
            test_space_policy(),
            |_, _, _, destination, _| {
                fs::create_dir(destination)?;
                fs::write(destination.join("new"), b"new")?;
                Ok(PublicationOutcome::PublishedParentSyncPending {
                    destination: destination.to_path_buf(),
                    message: "injected sync failure".into(),
                })
            },
        )
        .unwrap();
        assert!(matches!(
            outcome.publication,
            PublicationOutcome::PublishedParentSyncPending { .. }
        ));
        let quarantine = outcome.quarantine_cleanup_pending.unwrap();
        assert_eq!(fs::read(quarantine.join("prior/sentinel")).unwrap(), b"old");
        assert_eq!(fs::read(destination.join("new")).unwrap(), b"new");
        fs::remove_dir_all(destination).unwrap();
        fs::remove_dir_all(quarantine).unwrap();
    }
}
