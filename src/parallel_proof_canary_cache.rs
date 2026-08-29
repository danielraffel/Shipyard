//! Default-off cache-generation observation for a repository-scoped macOS canary.
//!
//! The production observer in this module only reads a local cache tree. It
//! creates a canonical immutable manifest by hashing the complete tree through
//! the artifact transport's no-follow verifier, compares that manifest with
//! controller policy, and emits typed zero-model evidence. A controller may
//! persist that evidence crash-durably, but this module cannot populate,
//! replace, delete, or remotely mutate a cache and does not execute a canary.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::artifact_transport::{LayoutEntry, verified_immutable_tree_inventory};
use crate::immutable_store::{ImmutableByteStore, ImmutableStoreError};
use crate::parallel_proof::{Sha256Digest, StoreWriteOutcome};
use crate::parallel_proof_canary::{
    CanaryCacheGeneration, CanaryRoute, PulpMacCanaryPolicy, canary_policy_scope_valid,
};
use crate::parallel_proof_canary_remote_cache::RemoteM1CacheAuthorityReceipt;

/// Current immutable cache manifest schema.
pub const CACHE_GENERATION_MANIFEST_SCHEMA: u32 = 1;
/// Current host cache-observation schema.
pub const CACHE_GENERATION_OBSERVATION_SCHEMA: u32 = 2;
/// Current paired M3/M1 evidence schema.
pub const PULP_MAC_CACHE_EVIDENCE_SCHEMA: u32 = 3;

const MAX_CACHE_ENTRIES: usize = 100_000;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVIDENCE_BYTES: usize = 40 * 1024 * 1024;
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

    pub(crate) fn host_observation_sha256(&self) -> &Sha256Digest {
        &self.host_observation_sha256
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn expected_manifest(&self) -> &CacheGenerationManifest {
        &self.expected_manifest
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
    /// Authenticated companion/transport authority for a remote observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_authority: Option<RemoteM1CacheAuthorityReceipt>,
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
            || match self.remote_authority.as_ref() {
                Some(authority) => {
                    authority.validate().is_err()
                        || authority.authority.host_id != self.host_id
                        || authority.authority.host_observation_sha256
                            != self.host_observation_sha256
                        || authority.authority.observed_at_ms > self.observed_at_ms
                        || !matches!(
                            authority.binds_cache_observation(
                                &self.host_observation_sha256,
                                &self.cache_root,
                                &self.manifest,
                                self.probe_elapsed_ms,
                            ),
                            Ok(true)
                        )
                }
                None => false,
            }
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
#[derive(Clone, Debug)]
pub struct LocalCacheGenerationObserver {
    host_id: String,
}

impl LocalCacheGenerationObserver {
    /// Bind local filesystem evidence to one explicitly configured host id.
    pub fn new(host_id: impl Into<String>) -> Result<Self, CacheObserverError> {
        let host_id = host_id.into();
        if !valid_label(&host_id) {
            return Err(CacheObserverError::Invalid(
                "local cache observer host id".to_owned(),
            ));
        }
        Ok(Self { host_id })
    }
}

impl CacheGenerationObserver for LocalCacheGenerationObserver {
    fn observe(
        &mut self,
        spec: &CacheGenerationProbeSpec,
    ) -> Result<CacheGenerationObservationReceipt, CacheObserverError> {
        if spec.host_id != self.host_id {
            return Err(CacheObserverError::Invalid(
                "local cache observer host binding".to_owned(),
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
            remote_authority: None,
            model_calls: 0,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    fn controller_now_ms(&mut self) -> Result<u64, CacheObserverError> {
        controller_now_ms()
    }
}

/// Default-off request for sequential builder-then-worker cache observation.
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

/// Complete paired cache evidence. Local receipts prove cache identity only;
/// a protected remote receipt may additionally prove its exact M1 session,
/// LAN route, and storage fences. Workload capabilities and canary execution
/// remain separate.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PulpMacCacheProbeEvidence {
    /// Evidence schema.
    pub schema_version: u32,
    /// Stable no-overwrite evidence identity.
    pub correlation_id: String,
    /// Immutable repository database identity bound by policy.
    pub repository_id: u64,
    /// Canonical repository slug bound by policy.
    pub repository: String,
    /// Exact Shipyard target bound by policy.
    pub target: String,
    /// Exact build target triple bound by policy.
    pub target_triple: String,
    /// Exact local builder host bound by policy.
    pub builder_host_id: String,
    /// Exact remote worker host bound by policy.
    pub worker_host_id: String,
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
            || !canary_policy_scope_valid(policy)
            || self.repository_id != policy.repository_id
            || self.repository != policy.repository
            || self.target != policy.target
            || self.target_triple != policy.target_triple
            || self.builder_host_id != policy.builder_host_id
            || self.worker_host_id != policy.worker_host_id
            || self.model_calls != 0
        {
            return Err(CacheObserverError::Invalid(
                "Pulp macOS cache evidence header".to_owned(),
            ));
        }
        validate_role_receipts(
            &self.builder,
            &policy.builder_host_id,
            &policy.required_cache_generations,
            Some((policy.assessed_at_ms, policy.maximum_observation_age_ms)),
            false,
        )?;
        validate_receipts_precede_assessment(&self.builder, self.assessed_at_ms)?;
        validate_role_receipts(
            &self.worker,
            &policy.worker_host_id,
            &policy.required_cache_generations,
            Some((policy.assessed_at_ms, policy.maximum_observation_age_ms)),
            true,
        )?;
        if self.worker.iter().any(|receipt| {
            receipt.remote_authority.as_ref().is_some_and(|authority| {
                authority.authority.source_host_id != policy.builder_host_id
            })
        }) {
            return Err(CacheObserverError::Invalid(
                "remote cache authority source host".to_owned(),
            ));
        }
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

    /// Return the singleton authenticated remote M1 authority when it binds
    /// the exact host receipt and current readiness assessment.
    #[must_use]
    pub fn remote_worker_authority(
        &self,
        policy: &PulpMacCanaryPolicy,
        worker_host_observation_sha256: &Sha256Digest,
        artifact_bytes_total: u64,
    ) -> Option<&RemoteM1CacheAuthorityReceipt> {
        if self.validate(policy).is_err() || self.worker.is_empty() {
            return None;
        }
        let mut authorities = self
            .worker
            .iter()
            .filter_map(|receipt| receipt.remote_authority.as_ref());
        let authority = authorities.next()?;
        if authority.authority.source_host_id != policy.builder_host_id
            || authority.authority.route != CanaryRoute::Lan
            || !authority.proves(
                worker_host_observation_sha256,
                artifact_bytes_total,
                policy.assessed_at_ms,
                policy.maximum_observation_age_ms,
            )
            || authorities.any(|candidate| {
                !candidate.has_same_controller_fence(authority)
                    || !candidate.proves(
                        worker_host_observation_sha256,
                        artifact_bytes_total,
                        policy.assessed_at_ms,
                        policy.maximum_observation_age_ms,
                    )
            })
        {
            return None;
        }
        Some(authority)
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
    store: ImmutableByteStore,
}

impl PulpMacCacheEvidenceStore {
    /// Create or reopen a controller-owned evidence directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, CacheObserverError> {
        Ok(Self {
            store: ImmutableByteStore::open(root, MAX_EVIDENCE_BYTES).map_err(map_store_error)?,
        })
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
        let evidence: PulpMacCacheProbeEvidence =
            serde_json::from_slice(&self.store.load(correlation_id).map_err(map_store_error)?)?;
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
        self.store
            .put(&evidence.correlation_id, &serde_json::to_vec(evidence)?)
            .map_err(map_store_error)
    }
}

fn map_store_error(error: ImmutableStoreError) -> CacheObserverError {
    match error {
        ImmutableStoreError::InvalidRoot => {
            CacheObserverError::Invalid("cache evidence root".to_owned())
        }
        ImmutableStoreError::UnsafePath(path) => {
            CacheObserverError::Invalid(format!("unsafe cache evidence path {}", path.display()))
        }
        ImmutableStoreError::LimitExceeded { .. } => {
            CacheObserverError::Invalid("cache evidence exceeds size limit".to_owned())
        }
        ImmutableStoreError::Missing(key) => CacheObserverError::Missing(key),
        ImmutableStoreError::Conflict(key) => CacheObserverError::ImmutableConflict(key),
        ImmutableStoreError::Io(error) => CacheObserverError::Io(error),
    }
}

/// Observe every required cache on the configured builder first, then the worker, and durably publish the
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
            if validate_receipt_bindings(&request.builder, &evidence.builder, false).is_err()
                || validate_receipt_bindings(&request.worker, &evidence.worker, true).is_err()
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
    validate_receipt_bindings(&request.builder, &builder, false)?;
    // The worker is never probed before every exact builder generation is proven.
    let builder_assessed_at_ms = observer.controller_now_ms()?;
    validate_role_receipts(
        &builder,
        &policy.builder_host_id,
        &policy.required_cache_generations,
        Some((builder_assessed_at_ms, policy.maximum_observation_age_ms)),
        false,
    )?;
    validate_receipts_precede_assessment(&builder, builder_assessed_at_ms)?;
    let worker = request
        .worker
        .iter()
        .map(|spec| observer.observe(spec))
        .collect::<Result<Vec<_>, _>>()?;
    validate_receipt_bindings(&request.worker, &worker, true)?;
    let assessed_at_ms = observer.controller_now_ms()?;
    let observation_policy = policy_at(policy, assessed_at_ms)?;
    let evidence = PulpMacCacheProbeEvidence {
        schema_version: PULP_MAC_CACHE_EVIDENCE_SCHEMA,
        correlation_id: request.correlation_id.clone(),
        repository_id: policy.repository_id,
        repository: policy.repository.clone(),
        target: policy.target.clone(),
        target_triple: policy.target_triple.clone(),
        builder_host_id: policy.builder_host_id.clone(),
        worker_host_id: policy.worker_host_id.clone(),
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

include!("parallel_proof_canary_cache/validation.rs");
#[cfg(test)]
mod tests;
