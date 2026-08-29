//! Default-off cache-generation observation for the Pulp macOS canary.
//!
//! The production observer in this module only reads a local cache tree. It
//! creates a canonical immutable manifest by hashing the complete tree through
//! the artifact transport's no-follow verifier, compares that manifest with
//! controller policy, and emits typed zero-model evidence. A controller may
//! persist that evidence crash-durably, but this module cannot populate,
//! replace, delete, or remotely mutate a cache and does not execute a canary.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::artifact_transport::{LayoutEntry, verified_immutable_tree_inventory};
use crate::parallel_proof::{Sha256Digest, StoreWriteOutcome};
use crate::parallel_proof_canary::{
    CanaryCacheGeneration, INITIAL_BUILDER, INITIAL_WORKER, PulpMacCanaryPolicy,
};

/// Current immutable cache manifest schema.
pub const CACHE_GENERATION_MANIFEST_SCHEMA: u32 = 1;
/// Current host cache-observation schema.
pub const CACHE_GENERATION_OBSERVATION_SCHEMA: u32 = 1;
/// Current paired M3/M1 evidence schema.
pub const PULP_MAC_CACHE_EVIDENCE_SCHEMA: u32 = 1;

const MAX_CACHE_ENTRIES: usize = 100_000;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVIDENCE_BYTES: usize = 40 * 1024 * 1024;
const MAX_EVIDENCE_BYTES_U64: u64 = MAX_EVIDENCE_BYTES as u64;
const MAX_CORRELATION_ID_BYTES: usize = 128;

/// One canonical entry in an immutable cache generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CacheGenerationEntry {
    /// Empty and non-empty directory identity, including portable mode.
    Directory {
        /// Normalized UTF-8 path relative to the cache root.
        path: String,
        /// Portable permission bits.
        mode: u32,
    },
    /// Complete regular-file identity.
    File {
        /// Normalized UTF-8 path relative to the cache root.
        path: String,
        /// Portable permission bits.
        mode: u32,
        /// Exact file size observed through the pinned file descriptor.
        size_bytes: u64,
        /// SHA-256 of the complete file contents.
        sha256: Sha256Digest,
    },
}

impl CacheGenerationEntry {
    fn path(&self) -> &str {
        match self {
            Self::Directory { path, .. } | Self::File { path, .. } => path,
        }
    }
}

/// Portable immutable cache contents, independent of the host-local root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheGenerationManifest {
    /// Manifest schema.
    pub schema_version: u32,
    /// Policy-facing exact cache identity.
    pub generation: CanaryCacheGeneration,
    /// Portable permission bits of the cache root itself.
    pub root_mode: u32,
    /// Sorted complete regular-file inventory.
    pub entries: Vec<CacheGenerationEntry>,
    /// Checked sum of all entry sizes.
    pub total_bytes: u64,
    /// Routine observation uses no model.
    pub model_calls: u64,
}

impl CacheGenerationManifest {
    /// Validate canonical ordering, byte arithmetic, and content identity.
    pub fn validate(&self) -> Result<(), CacheObserverError> {
        if self.schema_version != CACHE_GENERATION_MANIFEST_SCHEMA
            || self.model_calls != 0
            || !valid_label(&self.generation.name)
            || !valid_label(&self.generation.generation)
            || self.root_mode > 0o777
            || self.entries.is_empty()
            || self.entries.len() > MAX_CACHE_ENTRIES
        {
            return Err(CacheObserverError::Invalid(
                "cache generation manifest header".to_owned(),
            ));
        }
        let mut previous = None;
        let mut total = 0_u64;
        for entry in &self.entries {
            let path = entry.path();
            let valid_entry = match entry {
                CacheGenerationEntry::Directory { mode, .. } => *mode <= 0o777,
                CacheGenerationEntry::File {
                    mode,
                    size_bytes,
                    sha256,
                    ..
                } => {
                    total = total.checked_add(*size_bytes).ok_or_else(|| {
                        CacheObserverError::Invalid(
                            "cache generation byte total overflow".to_owned(),
                        )
                    })?;
                    *mode <= 0o777 && valid_sha256(sha256)
                }
            };
            if !valid_relative_path(path)
                || previous.is_some_and(|previous: &str| previous >= path)
                || !valid_entry
            {
                return Err(CacheObserverError::Invalid(
                    "cache generation manifest entries".to_owned(),
                ));
            }
            previous = Some(path);
        }
        let content_sha256 = cache_content_digest(self.root_mode, &self.entries)?;
        if total != self.total_bytes
            || content_sha256 != self.generation.sha256
            || serde_json::to_vec(&self.entries)?.len() > MAX_MANIFEST_BYTES
        {
            return Err(CacheObserverError::Invalid(
                "cache generation manifest content identity".to_owned(),
            ));
        }
        Ok(())
    }

    /// Domain-separated digest of the complete validated manifest.
    pub fn digest(&self) -> Result<Sha256Digest, CacheObserverError> {
        self.validate()?;
        domain_digest("shipyard.pulp-mac-cache.manifest.v1", self)
    }
}

/// Exact host-local cache generation expected by controller policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheGenerationProbeSpec {
    host_id: String,
    host_observation_sha256: Sha256Digest,
    root: PathBuf,
    expected_manifest: CacheGenerationManifest,
}

impl CacheGenerationProbeSpec {
    /// Construct a read-only probe without accessing the cache.
    pub fn new(
        host_id: impl Into<String>,
        host_observation_sha256: Sha256Digest,
        root: impl Into<PathBuf>,
        expected_manifest: CacheGenerationManifest,
    ) -> Result<Self, CacheObserverError> {
        let host_id = host_id.into();
        let root = root.into();
        expected_manifest.validate()?;
        if !valid_label(&host_id)
            || !valid_sha256(&host_observation_sha256)
            || !safe_absolute_cache_root(&root)
        {
            return Err(CacheObserverError::Invalid(
                "cache generation probe specification".to_owned(),
            ));
        }
        Ok(Self {
            host_id,
            host_observation_sha256,
            root,
            expected_manifest,
        })
    }

    /// Stable configured host identifier.
    #[must_use]
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// Required cache identity.
    #[must_use]
    pub fn generation(&self) -> &CanaryCacheGeneration {
        &self.expected_manifest.generation
    }
}

/// Immutable point-in-time proof of an exact cache tree on one host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheGenerationObservationReceipt {
    /// Receipt schema.
    pub schema_version: u32,
    /// Stable configured host identifier.
    pub host_id: String,
    /// Controller epoch time after the complete double observation.
    pub observed_at_ms: u64,
    /// Monotonic elapsed time for the complete double observation.
    pub probe_elapsed_ms: u64,
    /// Digest of the authenticated read-only host observation that authorized
    /// this filesystem probe.
    pub host_observation_sha256: Sha256Digest,
    /// Canonical host-local cache root that was read.
    pub cache_root: String,
    /// Complete portable generation manifest.
    pub manifest: CacheGenerationManifest,
    /// Digest of the complete manifest.
    pub manifest_sha256: Sha256Digest,
    /// Routine observation uses no model.
    pub model_calls: u64,
}

impl CacheGenerationObservationReceipt {
    /// Validate receipt identity and embedded immutable manifest.
    pub fn validate(&self) -> Result<(), CacheObserverError> {
        self.manifest.validate()?;
        if self.schema_version != CACHE_GENERATION_OBSERVATION_SCHEMA
            || !valid_label(&self.host_id)
            || self.observed_at_ms == 0
            || !valid_sha256(&self.host_observation_sha256)
            || !safe_absolute_cache_root(Path::new(&self.cache_root))
            || self.manifest.digest()? != self.manifest_sha256
            || self.probe_elapsed_ms == 0
            || self.model_calls != 0
        {
            return Err(CacheObserverError::Invalid(
                "cache generation observation receipt".to_owned(),
            ));
        }
        Ok(())
    }

    /// Domain-separated digest of the complete validated receipt.
    pub fn digest(&self) -> Result<Sha256Digest, CacheObserverError> {
        self.validate()?;
        domain_digest("shipyard.pulp-mac-cache.observation.v1", self)
    }
}

/// Produce a portable immutable manifest from one complete local cache tree.
///
/// The cache is read twice through no-follow handles and is never modified.
pub fn produce_cache_generation_manifest(
    root: &Path,
    name: impl Into<String>,
    generation: impl Into<String>,
) -> Result<CacheGenerationManifest, CacheObserverError> {
    let name = name.into();
    let generation = generation.into();
    if !valid_label(&name) || !valid_label(&generation) || !safe_absolute_cache_root(root) {
        return Err(CacheObserverError::Invalid(
            "cache generation producer input".to_owned(),
        ));
    }
    if fs::canonicalize(root)? != root {
        return Err(CacheObserverError::Invalid(
            "cache generation root must be canonical".to_owned(),
        ));
    }
    let inventory = verified_immutable_tree_inventory(root)
        .map_err(|error| CacheObserverError::Artifact(error.to_string()))?;
    let mut entries = Vec::new();
    for entry in inventory.entries {
        entries.push(match entry {
            LayoutEntry::Directory { path, mode } => CacheGenerationEntry::Directory { path, mode },
            LayoutEntry::File {
                path,
                mode,
                size_bytes,
                sha256,
            } => CacheGenerationEntry::File {
                path,
                mode,
                size_bytes,
                sha256: parse_sha256(&sha256)?,
            },
        });
    }
    if entries.is_empty() || entries.len() > MAX_CACHE_ENTRIES {
        return Err(CacheObserverError::Invalid(
            "cache generation entry count".to_owned(),
        ));
    }
    let total_bytes = entries.iter().try_fold(0_u64, |total, entry| match entry {
        CacheGenerationEntry::Directory { .. } => Ok(total),
        CacheGenerationEntry::File { size_bytes, .. } => {
            total.checked_add(*size_bytes).ok_or_else(|| {
                CacheObserverError::Invalid("cache generation byte total overflow".to_owned())
            })
        }
    })?;
    let manifest = CacheGenerationManifest {
        schema_version: CACHE_GENERATION_MANIFEST_SCHEMA,
        generation: CanaryCacheGeneration {
            name,
            generation,
            sha256: cache_content_digest(inventory.root_mode, &entries)?,
        },
        root_mode: inventory.root_mode,
        entries,
        total_bytes,
        model_calls: 0,
    };
    manifest.validate()?;
    Ok(manifest)
}

/// Read-only cache observer edge used by the default-off controller.
pub trait CacheGenerationObserver {
    /// Re-observe one exact cache generation without mutating it.
    fn observe(
        &mut self,
        spec: &CacheGenerationProbeSpec,
    ) -> Result<CacheGenerationObservationReceipt, CacheObserverError>;

    /// Controller epoch time sampled after all observations complete.
    fn controller_now_ms(&mut self) -> Result<u64, CacheObserverError>;
}

/// Production local observer. Remote observation requires a separately owned
/// authenticated companion/transport adapter and is intentionally absent.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalCacheGenerationObserver;

impl CacheGenerationObserver for LocalCacheGenerationObserver {
    fn observe(
        &mut self,
        spec: &CacheGenerationProbeSpec,
    ) -> Result<CacheGenerationObservationReceipt, CacheObserverError> {
        if spec.host_id != INITIAL_BUILDER {
            return Err(CacheObserverError::Invalid(
                "local cache observer may only attest the controller host m3".to_owned(),
            ));
        }
        let started = Instant::now();
        let actual = produce_cache_generation_manifest(
            &spec.root,
            spec.expected_manifest.generation.name.clone(),
            spec.expected_manifest.generation.generation.clone(),
        )?;
        if actual != spec.expected_manifest {
            return Err(CacheObserverError::GenerationMismatch {
                host_id: spec.host_id.clone(),
                cache_name: spec.expected_manifest.generation.name.clone(),
            });
        }
        let canonical = fs::canonicalize(&spec.root)?;
        if canonical != spec.root {
            return Err(CacheObserverError::Invalid(
                "cache root canonical identity drifted".to_owned(),
            ));
        }
        let receipt = CacheGenerationObservationReceipt {
            schema_version: CACHE_GENERATION_OBSERVATION_SCHEMA,
            host_id: spec.host_id.clone(),
            observed_at_ms: controller_now_ms()?,
            probe_elapsed_ms: milliseconds_ceil(started.elapsed())?,
            host_observation_sha256: spec.host_observation_sha256.clone(),
            cache_root: canonical
                .to_str()
                .ok_or_else(|| CacheObserverError::Invalid("cache root is not UTF-8".to_owned()))?
                .to_owned(),
            manifest_sha256: actual.digest()?,
            manifest: actual,
            model_calls: 0,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    fn controller_now_ms(&mut self) -> Result<u64, CacheObserverError> {
        controller_now_ms()
    }
}

/// Default-off request for sequential M3-then-M1 cache observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PulpMacCacheProbeRequest {
    /// Explicit opt-in. False performs no observation or write.
    pub enabled: bool,
    /// Stable no-overwrite evidence identity.
    pub correlation_id: String,
    /// All exact builder cache probes, sorted by generation name.
    pub builder: Vec<CacheGenerationProbeSpec>,
    /// All exact worker cache probes, sorted by generation name.
    pub worker: Vec<CacheGenerationProbeSpec>,
}

/// Complete paired cache evidence. This proves cache identity only; session,
/// route, capability, staging, reserve, and canary execution remain separate.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PulpMacCacheProbeEvidence {
    /// Evidence schema.
    pub schema_version: u32,
    /// Stable no-overwrite evidence identity.
    pub correlation_id: String,
    /// Controller policy time used for freshness.
    pub assessed_at_ms: u64,
    /// Builder receipts, always completed before worker observation begins.
    pub builder: Vec<CacheGenerationObservationReceipt>,
    /// Worker receipts.
    pub worker: Vec<CacheGenerationObservationReceipt>,
    /// Routine observation uses no model.
    pub model_calls: u64,
}

impl PulpMacCacheProbeEvidence {
    /// Validate exact host roles, cache inventory, freshness, and integrity.
    pub fn validate(&self, policy: &PulpMacCanaryPolicy) -> Result<(), CacheObserverError> {
        if self.schema_version != PULP_MAC_CACHE_EVIDENCE_SCHEMA
            || !valid_correlation_id(&self.correlation_id)
            || self.assessed_at_ms == 0
            || self.assessed_at_ms > policy.assessed_at_ms
            || policy.maximum_observation_age_ms == 0
            || policy.builder_host_id != INITIAL_BUILDER
            || policy.worker_host_id != INITIAL_WORKER
            || self.model_calls != 0
        {
            return Err(CacheObserverError::Invalid(
                "Pulp macOS cache evidence header".to_owned(),
            ));
        }
        validate_role_receipts(
            &self.builder,
            INITIAL_BUILDER,
            &policy.required_cache_generations,
            Some((policy.assessed_at_ms, policy.maximum_observation_age_ms)),
        )?;
        validate_receipts_precede_assessment(&self.builder, self.assessed_at_ms)?;
        validate_role_receipts(
            &self.worker,
            INITIAL_WORKER,
            &policy.required_cache_generations,
            Some((policy.assessed_at_ms, policy.maximum_observation_age_ms)),
        )?;
        validate_receipts_precede_assessment(&self.worker, self.assessed_at_ms)?;
        Ok(())
    }

    /// Whether this exact evidence closes only the cache-generation gap.
    #[must_use]
    pub fn proves_policy(&self, policy: &PulpMacCanaryPolicy) -> bool {
        self.validate(policy).is_ok()
    }

    /// Whether this evidence also binds the exact current read-only host
    /// observations used by the physical-readiness controller.
    #[must_use]
    pub fn proves_policy_and_hosts(
        &self,
        policy: &PulpMacCanaryPolicy,
        builder_host_observation_sha256: &Sha256Digest,
        worker_host_observation_sha256: &Sha256Digest,
    ) -> bool {
        self.validate(policy).is_ok()
            && role_host_digest(&self.builder, &policy.required_cache_generations)
                .is_ok_and(|digest| digest.as_ref() == Some(builder_host_observation_sha256))
            && role_host_digest(&self.worker, &policy.required_cache_generations)
                .is_ok_and(|digest| digest.as_ref() == Some(worker_host_observation_sha256))
    }

    /// Domain-separated digest of the validated evidence.
    pub fn digest(&self, policy: &PulpMacCanaryPolicy) -> Result<Sha256Digest, CacheObserverError> {
        self.validate(policy)?;
        domain_digest("shipyard.pulp-mac-cache.evidence.v1", self)
    }
}

/// Result of the default-off cache observation controller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PulpMacCacheProbeOutcome {
    /// No observer or store call occurred.
    Disabled,
    /// Exact evidence was created or replayed byte-identically.
    Recorded {
        /// Immutable paired evidence.
        evidence: Box<PulpMacCacheProbeEvidence>,
        /// Durable no-overwrite publication outcome.
        write_outcome: StoreWriteOutcome,
    },
}

/// Crash-durable no-overwrite store for paired cache evidence.
#[derive(Clone, Debug)]
pub struct PulpMacCacheEvidenceStore {
    root: PathBuf,
}

impl PulpMacCacheEvidenceStore {
    /// Create or reopen a controller-owned evidence directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, CacheObserverError> {
        let root = root.into();
        if root.file_name().is_none()
            || root
                .components()
                .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
        {
            return Err(CacheObserverError::Invalid(
                "cache evidence root".to_owned(),
            ));
        }
        let parent = root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !fs::symlink_metadata(parent)?.file_type().is_dir() {
            return Err(CacheObserverError::Invalid(
                "cache evidence root parent".to_owned(),
            ));
        }
        match fs::create_dir(&root) {
            Ok(()) => set_private_directory_permissions(&root)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&root)?;
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return Err(CacheObserverError::Invalid(
                        "cache evidence root must be a real directory".to_owned(),
                    ));
                }
                validate_private_directory_permissions(&metadata)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(Self { root })
    }

    /// Load and validate one exact evidence record.
    pub fn load(
        &self,
        correlation_id: &str,
        policy: &PulpMacCanaryPolicy,
    ) -> Result<PulpMacCacheProbeEvidence, CacheObserverError> {
        if !valid_correlation_id(correlation_id) {
            return Err(CacheObserverError::Invalid(
                "cache evidence correlation id".to_owned(),
            ));
        }
        let path = self.record_path(correlation_id);
        reject_non_regular_if_present(&path)?;
        if !path.exists() {
            return Err(CacheObserverError::Missing(correlation_id.to_owned()));
        }
        let evidence: PulpMacCacheProbeEvidence = serde_json::from_slice(&read_bounded(&path)?)?;
        evidence.validate(policy)?;
        if evidence.correlation_id != correlation_id {
            return Err(CacheObserverError::Invalid(
                "cache evidence logical key mismatch".to_owned(),
            ));
        }
        Ok(evidence)
    }

    fn record(
        &self,
        evidence: &PulpMacCacheProbeEvidence,
        policy: &PulpMacCanaryPolicy,
    ) -> Result<StoreWriteOutcome, CacheObserverError> {
        evidence.validate(policy)?;
        let bytes = serde_json::to_vec(evidence)?;
        if bytes.len() > MAX_EVIDENCE_BYTES {
            return Err(CacheObserverError::Invalid(
                "cache evidence exceeds size limit".to_owned(),
            ));
        }
        let destination = self.record_path(&evidence.correlation_id);
        let lock_path = destination.with_extension("lock");
        reject_non_regular_if_present(&destination)?;
        reject_non_regular_if_present(&lock_path)?;
        let lock = open_lock_nofollow(&lock_path)?;
        lock.lock_exclusive()?;
        let result = (|| {
            if destination.exists() {
                let existing = read_bounded(&destination)?;
                return if existing == bytes {
                    Ok(StoreWriteOutcome::AlreadyPresent)
                } else {
                    Err(CacheObserverError::ImmutableConflict(
                        evidence.correlation_id.clone(),
                    ))
                };
            }
            let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
            temporary.write_all(&bytes)?;
            temporary.as_file_mut().sync_all()?;
            match temporary.persist_noclobber(&destination) {
                Ok(_) => {
                    set_private_file_permissions(&destination)?;
                    sync_directory(&self.root)?;
                    Ok(StoreWriteOutcome::Created)
                }
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let existing = read_bounded(&destination)?;
                    if existing == bytes {
                        Ok(StoreWriteOutcome::AlreadyPresent)
                    } else {
                        Err(CacheObserverError::ImmutableConflict(
                            evidence.correlation_id.clone(),
                        ))
                    }
                }
                Err(error) => Err(error.error.into()),
            }
        })();
        FileExt::unlock(&lock)?;
        result
    }

    fn record_path(&self, correlation_id: &str) -> PathBuf {
        self.root.join(format!(
            "{}.json",
            Sha256Digest::of_bytes(correlation_id.as_bytes()).as_str()
        ))
    }
}

/// Observe every required cache on M3 first, then M1, and durably publish the
/// complete evidence. The explicit disabled path performs no observer call.
pub fn drive_pulp_mac_cache_probe<O: CacheGenerationObserver>(
    request: &PulpMacCacheProbeRequest,
    policy: &PulpMacCanaryPolicy,
    observer: &mut O,
    store: &PulpMacCacheEvidenceStore,
) -> Result<PulpMacCacheProbeOutcome, CacheObserverError> {
    if !request.enabled || !policy.enabled {
        return Ok(PulpMacCacheProbeOutcome::Disabled);
    }
    validate_request(request, policy)?;
    let replay_assessed_at_ms = observer.controller_now_ms()?;
    let replay_policy = policy_at(policy, replay_assessed_at_ms)?;
    match store.load(&request.correlation_id, &replay_policy) {
        Ok(evidence) => {
            if validate_receipt_bindings(&request.builder, &evidence.builder).is_err()
                || validate_receipt_bindings(&request.worker, &evidence.worker).is_err()
            {
                return Err(CacheObserverError::ImmutableConflict(
                    request.correlation_id.clone(),
                ));
            }
            return Ok(PulpMacCacheProbeOutcome::Recorded {
                evidence: Box::new(evidence),
                write_outcome: StoreWriteOutcome::AlreadyPresent,
            });
        }
        Err(CacheObserverError::Missing(_)) => {}
        Err(error) => return Err(error),
    }
    let builder = request
        .builder
        .iter()
        .map(|spec| observer.observe(spec))
        .collect::<Result<Vec<_>, _>>()?;
    validate_receipt_bindings(&request.builder, &builder)?;
    // M1 is never probed before every exact M3 generation has been proven.
    let builder_assessed_at_ms = observer.controller_now_ms()?;
    validate_role_receipts(
        &builder,
        INITIAL_BUILDER,
        &policy.required_cache_generations,
        Some((builder_assessed_at_ms, policy.maximum_observation_age_ms)),
    )?;
    validate_receipts_precede_assessment(&builder, builder_assessed_at_ms)?;
    let worker = request
        .worker
        .iter()
        .map(|spec| observer.observe(spec))
        .collect::<Result<Vec<_>, _>>()?;
    validate_receipt_bindings(&request.worker, &worker)?;
    let assessed_at_ms = observer.controller_now_ms()?;
    let observation_policy = policy_at(policy, assessed_at_ms)?;
    let evidence = PulpMacCacheProbeEvidence {
        schema_version: PULP_MAC_CACHE_EVIDENCE_SCHEMA,
        correlation_id: request.correlation_id.clone(),
        assessed_at_ms,
        builder,
        worker,
        model_calls: 0,
    };
    evidence.validate(&observation_policy)?;
    let write_outcome = store.record(&evidence, &observation_policy)?;
    Ok(PulpMacCacheProbeOutcome::Recorded {
        evidence: Box::new(evidence),
        write_outcome,
    })
}

/// Cache observation failure.
#[derive(Debug)]
pub enum CacheObserverError {
    /// Configuration or serialized evidence violated the contract.
    Invalid(String),
    /// Current cache contents differ from immutable controller policy.
    GenerationMismatch {
        /// Stable host identifier.
        host_id: String,
        /// Stable cache family.
        cache_name: String,
    },
    /// No durable evidence exists for the logical key.
    Missing(String),
    /// A different immutable record already owns the logical key.
    ImmutableConflict(String),
    /// Artifact tree verification refused the cache.
    Artifact(String),
    /// Filesystem failure.
    Io(std::io::Error),
    /// Canonical JSON failure.
    Json(serde_json::Error),
}

impl fmt::Display for CacheObserverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Artifact(message) => formatter.write_str(message),
            Self::GenerationMismatch {
                host_id,
                cache_name,
            } => write!(
                formatter,
                "host {host_id} cache {cache_name} does not match its immutable generation"
            ),
            Self::Missing(key) => write!(formatter, "cache evidence {key} is absent"),
            Self::ImmutableConflict(key) => {
                write!(
                    formatter,
                    "cache evidence {key} conflicts with an existing record"
                )
            }
            Self::Io(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CacheObserverError {}

impl From<std::io::Error> for CacheObserverError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for CacheObserverError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

fn validate_request(
    request: &PulpMacCacheProbeRequest,
    policy: &PulpMacCanaryPolicy,
) -> Result<(), CacheObserverError> {
    if !valid_correlation_id(&request.correlation_id)
        || policy.maximum_observation_age_ms == 0
        || policy.builder_host_id != INITIAL_BUILDER
        || policy.worker_host_id != INITIAL_WORKER
    {
        return Err(CacheObserverError::Invalid(
            "Pulp macOS cache probe request".to_owned(),
        ));
    }
    validate_specs(
        &request.builder,
        INITIAL_BUILDER,
        &policy.required_cache_generations,
    )?;
    validate_specs(
        &request.worker,
        INITIAL_WORKER,
        &policy.required_cache_generations,
    )
}

fn validate_specs(
    specs: &[CacheGenerationProbeSpec],
    host_id: &str,
    required: &[CanaryCacheGeneration],
) -> Result<(), CacheObserverError> {
    let generations = specs
        .iter()
        .map(|spec| {
            spec.expected_manifest.validate()?;
            if spec.host_id != host_id {
                return Err(CacheObserverError::Invalid(
                    "cache probe host role".to_owned(),
                ));
            }
            Ok(spec.expected_manifest.generation.clone())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if generations != required || !generations_canonical(&generations) {
        return Err(CacheObserverError::Invalid(
            "cache probe required generations".to_owned(),
        ));
    }
    let host_digests = specs
        .iter()
        .map(|spec| &spec.host_observation_sha256)
        .collect::<BTreeSet<_>>();
    if (!specs.is_empty() && host_digests.len() != 1)
        || host_digests.iter().any(|digest| !valid_sha256(digest))
    {
        return Err(CacheObserverError::Invalid(
            "cache probe host observation binding".to_owned(),
        ));
    }
    Ok(())
}

fn validate_receipt_bindings(
    specs: &[CacheGenerationProbeSpec],
    receipts: &[CacheGenerationObservationReceipt],
) -> Result<(), CacheObserverError> {
    if specs.len() != receipts.len()
        || specs.iter().zip(receipts).any(|(spec, receipt)| {
            receipt.host_id != spec.host_id
                || receipt.host_observation_sha256 != spec.host_observation_sha256
                || receipt.manifest != spec.expected_manifest
                || receipt.cache_root != spec.root.to_str().unwrap_or_default()
        })
    {
        return Err(CacheObserverError::Invalid(
            "cache observation request binding".to_owned(),
        ));
    }
    Ok(())
}

fn validate_receipts_precede_assessment(
    receipts: &[CacheGenerationObservationReceipt],
    assessed_at_ms: u64,
) -> Result<(), CacheObserverError> {
    if receipts
        .iter()
        .any(|receipt| receipt.observed_at_ms > assessed_at_ms)
    {
        return Err(CacheObserverError::Invalid(
            "cache receipt postdates its evidence assessment".to_owned(),
        ));
    }
    Ok(())
}

fn policy_at(
    policy: &PulpMacCanaryPolicy,
    assessed_at_ms: u64,
) -> Result<PulpMacCanaryPolicy, CacheObserverError> {
    if assessed_at_ms == 0 || assessed_at_ms < policy.assessed_at_ms {
        return Err(CacheObserverError::Invalid(
            "cache controller assessment clock regressed".to_owned(),
        ));
    }
    let mut current = policy.clone();
    current.assessed_at_ms = assessed_at_ms;
    Ok(current)
}

fn validate_role_receipts(
    receipts: &[CacheGenerationObservationReceipt],
    host_id: &str,
    required: &[CanaryCacheGeneration],
    freshness: Option<(u64, u64)>,
) -> Result<(), CacheObserverError> {
    let generations = receipts
        .iter()
        .map(|receipt| {
            receipt.validate()?;
            let stale = freshness.is_some_and(|(assessed_at_ms, maximum_age_ms)| {
                assessed_at_ms == 0
                    || maximum_age_ms == 0
                    || receipt.observed_at_ms > assessed_at_ms
                    || assessed_at_ms.saturating_sub(receipt.observed_at_ms) > maximum_age_ms
            });
            if receipt.host_id != host_id || stale {
                return Err(CacheObserverError::Invalid(
                    "cache observation host or freshness fence".to_owned(),
                ));
            }
            Ok(receipt.manifest.generation.clone())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if generations != required || !generations_canonical(&generations) {
        return Err(CacheObserverError::Invalid(
            "cache observation generation inventory".to_owned(),
        ));
    }
    Ok(())
}

fn role_host_digest(
    receipts: &[CacheGenerationObservationReceipt],
    required: &[CanaryCacheGeneration],
) -> Result<Option<Sha256Digest>, CacheObserverError> {
    if required.is_empty() {
        return Ok(None);
    }
    let digests = receipts
        .iter()
        .map(|receipt| receipt.host_observation_sha256.clone())
        .collect::<BTreeSet<_>>();
    if digests.len() != 1 {
        return Err(CacheObserverError::Invalid(
            "cache observation host digest inventory".to_owned(),
        ));
    }
    Ok(digests.into_iter().next())
}

fn generations_canonical(generations: &[CanaryCacheGeneration]) -> bool {
    generations
        .windows(2)
        .all(|pair| pair[0].name < pair[1].name)
        && generations.iter().all(|generation| {
            valid_label(&generation.name)
                && valid_label(&generation.generation)
                && valid_sha256(&generation.sha256)
        })
}

fn cache_content_digest(
    root_mode: u32,
    entries: &[CacheGenerationEntry],
) -> Result<Sha256Digest, CacheObserverError> {
    domain_digest(
        "shipyard.cache-generation.contents.v1",
        &(root_mode, entries),
    )
}

fn domain_digest<T: Serialize + ?Sized>(
    domain: &str,
    value: &T,
) -> Result<Sha256Digest, CacheObserverError> {
    let payload = serde_json::to_vec(value)?;
    let mut bytes = Vec::with_capacity(16 + domain.len() + payload.len());
    bytes.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    bytes.extend_from_slice(domain.as_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    bytes.extend_from_slice(&payload);
    Ok(Sha256Digest::of_bytes(&bytes))
}

fn parse_sha256(value: &str) -> Result<Sha256Digest, CacheObserverError> {
    Sha256Digest::parse(value.to_owned())
        .map_err(|_| CacheObserverError::Invalid("cache entry SHA-256".to_owned()))
}

fn valid_sha256(value: &Sha256Digest) -> bool {
    value.as_str().len() == 64
        && value
            .as_str()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn valid_correlation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CORRELATION_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn safe_absolute_cache_root(path: &Path) -> bool {
    let Some(value) = path.to_str() else {
        return false;
    };
    path.is_absolute()
        && value != "/"
        && !value.ends_with('/')
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && component != "." && component != "..")
        && [
            "/tmp",
            "/private/tmp",
            "/var/tmp",
            "/private/var/tmp",
            "/var/folders",
            "/private/var/folders",
        ]
        .iter()
        .all(|temporary| value != *temporary && !value.starts_with(&format!("{temporary}/")))
}

fn controller_now_ms() -> Result<u64, CacheObserverError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CacheObserverError::Invalid(error.to_string()))?
        .as_millis();
    u64::try_from(millis)
        .map_err(|_| CacheObserverError::Invalid("controller clock overflow".to_owned()))
}

fn milliseconds_ceil(duration: Duration) -> Result<u64, CacheObserverError> {
    let millis = duration.as_millis();
    let millis = if duration.subsec_nanos().is_multiple_of(1_000_000) {
        millis
    } else {
        millis.checked_add(1).ok_or_else(|| {
            CacheObserverError::Invalid("cache observation duration overflow".to_owned())
        })?
    };
    u64::try_from(millis)
        .map_err(|_| CacheObserverError::Invalid("cache observation duration overflow".to_owned()))
}

fn reject_non_regular_if_present(path: &Path) -> Result<(), CacheObserverError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(CacheObserverError::Invalid(format!(
            "{} is not a regular file",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, CacheObserverError> {
    let file = open_readonly_nofollow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_EVIDENCE_BYTES_U64 {
        return Err(CacheObserverError::Invalid(
            "cache evidence exceeds size limit".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_EVIDENCE_BYTES_U64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_EVIDENCE_BYTES {
        return Err(CacheObserverError::Invalid(
            "cache evidence exceeds size limit".to_owned(),
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_readonly_nofollow(path: &Path) -> Result<File, CacheObserverError> {
    use rustix::fs::{Mode, OFlags, open};

    Ok(File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| CacheObserverError::Io(error.into()))?,
    ))
}

#[cfg(not(unix))]
fn open_readonly_nofollow(path: &Path) -> Result<File, CacheObserverError> {
    Ok(File::open(path)?)
}

#[cfg(unix)]
fn open_lock_nofollow(path: &Path) -> Result<File, CacheObserverError> {
    use rustix::fs::{Mode, OFlags, open};

    Ok(File::from(
        open(
            path,
            OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| CacheObserverError::Io(error.into()))?,
    ))
}

#[cfg(not(unix))]
fn open_lock_nofollow(path: &Path) -> Result<File, CacheObserverError> {
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), CacheObserverError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), CacheObserverError> {
    Ok(())
}

#[cfg(unix)]
fn validate_private_directory_permissions(
    metadata: &fs::Metadata,
) -> Result<(), CacheObserverError> {
    use std::os::unix::fs::PermissionsExt;

    if metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(CacheObserverError::Invalid(
            "cache evidence root must have mode 0700".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_directory_permissions(
    _metadata: &fs::Metadata,
) -> Result<(), CacheObserverError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), CacheObserverError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), CacheObserverError> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CacheObserverError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), CacheObserverError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use tempfile::TempDir;

    use super::*;

    fn persistent_temp() -> TempDir {
        tempfile::Builder::new()
            .prefix(".shipyard-cache-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap()
    }

    fn cache_tree() -> TempDir {
        let root = persistent_temp();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("index.bin"), b"cache-index").unwrap();
        fs::write(root.path().join("nested/object.bin"), b"cache-object").unwrap();
        root
    }

    fn policy(manifest: &CacheGenerationManifest) -> PulpMacCanaryPolicy {
        PulpMacCanaryPolicy {
            enabled: true,
            assessed_at_ms: 1_000,
            maximum_observation_age_ms: 100,
            required_cache_generations: vec![manifest.generation.clone()],
            ..PulpMacCanaryPolicy::default()
        }
    }

    fn host_digest(host_id: &str) -> Sha256Digest {
        Sha256Digest::of_bytes(format!("host:{host_id}").as_bytes())
    }

    fn receipt(
        host_id: &str,
        root: &Path,
        manifest: CacheGenerationManifest,
        observed_at_ms: u64,
    ) -> CacheGenerationObservationReceipt {
        CacheGenerationObservationReceipt {
            schema_version: CACHE_GENERATION_OBSERVATION_SCHEMA,
            host_id: host_id.to_owned(),
            host_observation_sha256: host_digest(host_id),
            observed_at_ms,
            probe_elapsed_ms: 1,
            cache_root: root.to_str().unwrap().to_owned(),
            manifest_sha256: manifest.digest().unwrap(),
            manifest,
            model_calls: 0,
        }
    }

    #[derive(Default)]
    struct FakeObserver {
        outputs: VecDeque<Result<CacheGenerationObservationReceipt, CacheObserverError>>,
        calls: Vec<String>,
        now_ms: u64,
    }

    impl CacheGenerationObserver for FakeObserver {
        fn observe(
            &mut self,
            spec: &CacheGenerationProbeSpec,
        ) -> Result<CacheGenerationObservationReceipt, CacheObserverError> {
            self.calls.push(spec.host_id().to_owned());
            self.outputs.pop_front().expect("fake cache observation")
        }

        fn controller_now_ms(&mut self) -> Result<u64, CacheObserverError> {
            Ok(self.now_ms)
        }
    }

    #[test]
    fn manifest_is_deterministic_complete_and_read_only() {
        let root = cache_tree();
        let before = fs::read(root.path().join("index.bin")).unwrap();
        let first = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let second = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        assert_eq!(first, second);
        assert_eq!(first.entries.len(), 3);
        assert_eq!(first.total_bytes, 23);
        assert_eq!(first.model_calls, 0);
        assert_eq!(fs::read(root.path().join("index.bin")).unwrap(), before);
        first.validate().unwrap();

        fs::write(root.path().join("nested/object.bin"), b"different-object").unwrap();
        let changed = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        assert_ne!(first.generation.sha256, changed.generation.sha256);
    }

    #[cfg(unix)]
    #[test]
    fn manifest_identity_includes_the_cache_root_mode() {
        use std::os::unix::fs::PermissionsExt;

        let root = cache_tree();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let private = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap();
        let shared = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        assert_eq!(private.root_mode, 0o700);
        assert_eq!(shared.root_mode, 0o750);
        assert_ne!(private.generation.sha256, shared.generation.sha256);
    }

    #[cfg(unix)]
    #[test]
    fn manifest_rejects_links_and_special_or_empty_trees() {
        use std::os::unix::fs::symlink;

        let empty = persistent_temp();
        assert!(produce_cache_generation_manifest(empty.path(), "skia", "m124").is_err());

        let linked = persistent_temp();
        fs::write(linked.path().join("target"), b"target").unwrap();
        symlink("target", linked.path().join("link")).unwrap();
        assert!(produce_cache_generation_manifest(linked.path(), "skia", "m124").is_err());
    }

    #[test]
    fn local_observer_requires_the_exact_immutable_manifest() {
        let root = cache_tree();
        let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let spec =
            CacheGenerationProbeSpec::new("m3", host_digest("m3"), root.path(), manifest.clone())
                .unwrap();
        let observed = LocalCacheGenerationObserver.observe(&spec).unwrap();
        assert_eq!(observed.manifest, manifest);
        assert_eq!(observed.model_calls, 0);
        observed.validate().unwrap();

        let worker_spec =
            CacheGenerationProbeSpec::new("m1", host_digest("m1"), root.path(), manifest.clone())
                .unwrap();
        assert!(LocalCacheGenerationObserver.observe(&worker_spec).is_err());

        fs::write(root.path().join("index.bin"), b"drifted").unwrap();
        assert!(matches!(
            LocalCacheGenerationObserver.observe(&spec),
            Err(CacheObserverError::GenerationMismatch { .. })
        ));
    }

    #[test]
    fn disabled_probe_calls_neither_observer_nor_cache() {
        let parent = persistent_temp();
        let store = PulpMacCacheEvidenceStore::open(parent.path().join("evidence")).unwrap();
        let request = PulpMacCacheProbeRequest {
            enabled: false,
            correlation_id: "disabled".to_owned(),
            builder: Vec::new(),
            worker: Vec::new(),
        };
        let mut observer = FakeObserver::default();
        let outcome = drive_pulp_mac_cache_probe(
            &request,
            &PulpMacCanaryPolicy::default(),
            &mut observer,
            &store,
        )
        .unwrap();
        assert_eq!(outcome, PulpMacCacheProbeOutcome::Disabled);
        assert!(observer.calls.is_empty());
    }

    #[test]
    fn probe_is_m3_first_crash_durable_and_exactly_replayable() {
        let builder_root = cache_tree();
        let worker_root = cache_tree();
        let manifest =
            produce_cache_generation_manifest(builder_root.path(), "skia", "m124").unwrap();
        assert_eq!(
            manifest,
            produce_cache_generation_manifest(worker_root.path(), "skia", "m124").unwrap()
        );
        let policy = policy(&manifest);
        let request = PulpMacCacheProbeRequest {
            enabled: true,
            correlation_id: "cache-proof-1".to_owned(),
            builder: vec![
                CacheGenerationProbeSpec::new(
                    "m3",
                    host_digest("m3"),
                    builder_root.path(),
                    manifest.clone(),
                )
                .unwrap(),
            ],
            worker: vec![
                CacheGenerationProbeSpec::new(
                    "m1",
                    host_digest("m1"),
                    worker_root.path(),
                    manifest.clone(),
                )
                .unwrap(),
            ],
        };
        let mut observer = FakeObserver {
            outputs: VecDeque::from([
                Ok(receipt("m3", builder_root.path(), manifest.clone(), 990)),
                Ok(receipt("m1", worker_root.path(), manifest, 995)),
            ]),
            now_ms: 1_000,
            ..FakeObserver::default()
        };
        let parent = persistent_temp();
        let store = PulpMacCacheEvidenceStore::open(parent.path().join("evidence")).unwrap();
        let PulpMacCacheProbeOutcome::Recorded {
            evidence,
            write_outcome,
        } = drive_pulp_mac_cache_probe(&request, &policy, &mut observer, &store).unwrap()
        else {
            panic!("expected recorded cache proof");
        };
        assert_eq!(write_outcome, StoreWriteOutcome::Created);
        assert_eq!(observer.calls, ["m3", "m1"]);
        assert!(evidence.proves_policy(&policy));
        assert!(evidence.proves_policy_and_hosts(&policy, &host_digest("m3"), &host_digest("m1")));
        assert_eq!(evidence.model_calls, 0);
        evidence.digest(&policy).unwrap();

        let replay = drive_pulp_mac_cache_probe(&request, &policy, &mut observer, &store).unwrap();
        assert!(matches!(
            replay,
            PulpMacCacheProbeOutcome::Recorded {
                write_outcome: StoreWriteOutcome::AlreadyPresent,
                ..
            }
        ));
        assert_eq!(observer.calls, ["m3", "m1"]);
        assert_eq!(store.load("cache-proof-1", &policy).unwrap(), *evidence);

        let mut rebound_request = request;
        rebound_request.builder[0].host_observation_sha256 =
            Sha256Digest::of_bytes(b"new-m3-host-observation");
        assert!(matches!(
            drive_pulp_mac_cache_probe(&rebound_request, &policy, &mut observer, &store),
            Err(CacheObserverError::ImmutableConflict(key)) if key == "cache-proof-1"
        ));
        assert_eq!(observer.calls, ["m3", "m1"]);
    }

    #[test]
    fn failed_builder_proof_never_observes_worker() {
        let root = cache_tree();
        let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let request = PulpMacCacheProbeRequest {
            enabled: true,
            correlation_id: "builder-failed".to_owned(),
            builder: vec![
                CacheGenerationProbeSpec::new(
                    "m3",
                    host_digest("m3"),
                    root.path(),
                    manifest.clone(),
                )
                .unwrap(),
            ],
            worker: vec![
                CacheGenerationProbeSpec::new(
                    "m1",
                    host_digest("m1"),
                    root.path(),
                    manifest.clone(),
                )
                .unwrap(),
            ],
        };
        let mut observer = FakeObserver {
            outputs: VecDeque::from([Err(CacheObserverError::GenerationMismatch {
                host_id: "m3".to_owned(),
                cache_name: "skia".to_owned(),
            })]),
            now_ms: 1_000,
            ..FakeObserver::default()
        };
        let parent = persistent_temp();
        let store = PulpMacCacheEvidenceStore::open(parent.path().join("evidence")).unwrap();
        assert!(
            drive_pulp_mac_cache_probe(&request, &policy(&manifest), &mut observer, &store)
                .is_err()
        );
        assert_eq!(observer.calls, ["m3"]);
    }

    #[test]
    fn stale_builder_proof_never_observes_worker() {
        let root = cache_tree();
        let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let request = PulpMacCacheProbeRequest {
            enabled: true,
            correlation_id: "builder-stale".to_owned(),
            builder: vec![
                CacheGenerationProbeSpec::new(
                    "m3",
                    host_digest("m3"),
                    root.path(),
                    manifest.clone(),
                )
                .unwrap(),
            ],
            worker: vec![
                CacheGenerationProbeSpec::new(
                    "m1",
                    host_digest("m1"),
                    root.path(),
                    manifest.clone(),
                )
                .unwrap(),
            ],
        };
        let mut observer = FakeObserver {
            outputs: VecDeque::from([
                Ok(receipt("m3", root.path(), manifest.clone(), 899)),
                Ok(receipt("m1", root.path(), manifest.clone(), 995)),
            ]),
            now_ms: 1_000,
            ..FakeObserver::default()
        };
        let parent = persistent_temp();
        let store = PulpMacCacheEvidenceStore::open(parent.path().join("evidence")).unwrap();
        assert!(
            drive_pulp_mac_cache_probe(&request, &policy(&manifest), &mut observer, &store)
                .is_err()
        );
        assert_eq!(observer.calls, ["m3"]);
    }

    #[test]
    fn stale_or_wrong_inventory_never_proves_policy() {
        let root = cache_tree();
        let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let policy = policy(&manifest);
        let mut evidence = PulpMacCacheProbeEvidence {
            schema_version: PULP_MAC_CACHE_EVIDENCE_SCHEMA,
            correlation_id: "stale".to_owned(),
            assessed_at_ms: 1_000,
            builder: vec![receipt("m3", root.path(), manifest.clone(), 899)],
            worker: vec![receipt("m1", root.path(), manifest, 995)],
            model_calls: 0,
        };
        assert!(!evidence.proves_policy(&policy));
        evidence.builder[0].observed_at_ms = 990;
        evidence.worker[0].manifest.generation.generation = "other".to_owned();
        assert!(!evidence.proves_policy(&policy));

        evidence.worker[0].manifest.generation.generation = "m124".to_owned();
        evidence.worker[0].manifest_sha256 = evidence.worker[0].manifest.digest().unwrap();
        let later_policy = PulpMacCanaryPolicy {
            assessed_at_ms: 1_100,
            ..policy
        };
        assert!(!evidence.proves_policy(&later_policy));
    }
}
