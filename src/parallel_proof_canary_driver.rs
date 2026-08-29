//! Controller-owned execution and immutable publication for the default-off
//! repository-scoped macOS shadow canary.
//!
//! There is deliberately no production host adapter in this module. Enabling
//! policy without explicitly supplying an authenticated adapter cannot touch a
//! host, cache, or artifact. The adapter boundary exists so a later physical
//! canary can bind controller-authenticated observations and OS-reported byte
//! counters without putting shell execution in the daemon or receipt model.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::immutable_store::{ImmutableByteStore, ImmutableStoreError};
use crate::parallel_proof::{
    ParallelProofContext, ParallelProofError, Sha256Digest, StoreWriteOutcome,
};
use crate::parallel_proof_canary::{
    CanaryCacheGeneration, CanaryHostObservation, CanaryTimingEstimate, PulpMacCanaryDecision,
    PulpMacCanaryPolicy, assess_pulp_mac_canary, canary_host_observations_digest,
};
use crate::parallel_proof_canary_receipt::{
    ArtifactDeliveryMode, CacheUse, CanaryCacheMeasurement, CanaryMeasurementInput,
    PulpMacCanaryMeasurementReceipt, SingleHostControlReceipt, validate_correlation_id,
};

const DRIVER_SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;

/// Controller-authenticated use of one exact cache generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedCacheUse {
    /// Exact admitted cache generation.
    pub generation: CanaryCacheGeneration,
    /// Whether execution actually opened this generation.
    pub usage: CacheUse,
}

/// Evidence that a partial artifact survived an interruption and was verified
/// before the remaining suffix was transferred.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InterruptedTransferEvidence {
    /// Digest of the partial bytes as observed immediately after interruption.
    pub interrupted_partial_sha256: Sha256Digest,
    /// Digest of the authenticated prefix accepted by resume planning.
    pub verified_prefix_sha256: Sha256Digest,
    /// Bytes written before the interruption.
    pub bytes_before_interruption: u64,
    /// Verified prefix retained for the resumed attempt.
    pub verified_resume_offset_bytes: u64,
    /// New suffix bytes written after resumption.
    pub bytes_after_resume: u64,
}

/// Exact controller/transport counters for artifact delivery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDeliveryObservation {
    /// Delivery behavior proven by the transport adapter.
    pub mode: ArtifactDeliveryMode,
    /// Manifest-bound encoded artifact size.
    pub artifact_bytes_total: u64,
    /// Bytes already present and cryptographically verified for this attempt.
    pub artifact_bytes_reused: u64,
    /// Bytes newly transferred for this attempt, from transport counters.
    pub artifact_bytes_transferred: u64,
    /// Optional evidence from a deliberately interrupted predecessor transfer.
    pub interruption: Option<InterruptedTransferEvidence>,
}

/// Result of the transfer plus distributed shadow execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DistributedExecutionObservation {
    /// Exact transport byte and interruption counters.
    pub delivery: ArtifactDeliveryObservation,
    /// Controller-measured setup duration.
    pub setup_ms: u64,
    /// Controller-measured transfer duration.
    pub transfer_ms: u64,
    /// Controller-measured verification duration.
    pub verification_ms: u64,
    /// Controller-measured dispatch and aggregation duration.
    pub dispatch_ms: u64,
    /// Controller-measured critical-path shard duration.
    pub shard_execution_ms: u64,
    /// Sum of controller-measured active worker time.
    pub worker_active_ms: u64,
    /// Controller-measured submit-to-receipt duration.
    pub submit_to_receipt_ms: u64,
    /// Exact admitted cache generations actually used or observed unused.
    pub caches: Vec<ObservedCacheUse>,
}

/// Explicit controller adapter required to observe or execute a physical canary.
///
/// Implementations must authenticate host identity/session generation and must
/// obtain byte counters from the transport itself. There is intentionally no
/// shell-backed implementation in the library.
pub trait PulpMacCanaryExecutor {
    /// Read the controller's current UTC epoch milliseconds.
    fn controller_now_ms(&mut self) -> Result<u64, ParallelProofError>;

    /// Capture controller-authenticated builder/worker observations without mutation.
    fn authenticated_host_observations(
        &mut self,
    ) -> Result<Vec<CanaryHostObservation>, ParallelProofError>;

    /// Execute the single-host builder control and return its authenticated receipt.
    /// The adapter, not this driver, owns execution and timing authentication.
    fn run_single_host_control(
        &mut self,
        proof: ParallelProofContext<'_>,
        host: &CanaryHostObservation,
    ) -> Result<SingleHostControlReceipt, ParallelProofError>;

    /// Execute transfer/resume and distributed shadow work after control ends.
    fn run_distributed_shadow(
        &mut self,
        manifest_digest: &Sha256Digest,
    ) -> Result<DistributedExecutionObservation, ParallelProofError>;
}

/// Immutable typed evidence retained by the controller.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PulpMacCanaryEvidence {
    /// Driver evidence schema.
    pub schema_version: u32,
    /// Exact immutable measurement receipt.
    pub receipt: PulpMacCanaryMeasurementReceipt,
    /// Digest of the complete receipt.
    pub receipt_sha256: Sha256Digest,
    /// Complete authenticated observations immediately before distributed work.
    pub pre_execution_host_observations_sha256: Sha256Digest,
    /// Canonical authenticated M3/M1 observations before distributed work.
    pub pre_execution_host_observations: Vec<CanaryHostObservation>,
    /// Complete authenticated observations immediately before publication.
    pub final_host_observations_sha256: Sha256Digest,
    /// Canonical authenticated M3/M1 observations before publication.
    pub final_host_observations: Vec<CanaryHostObservation>,
    /// Optional authenticated interruption/resume proof.
    pub interrupted_transfer: Option<InterruptedTransferEvidence>,
    /// Model calls made by the controller driver; structurally always zero.
    pub model_calls: u64,
}

impl PulpMacCanaryEvidence {
    fn new(
        receipt: PulpMacCanaryMeasurementReceipt,
        interrupted_transfer: Option<InterruptedTransferEvidence>,
        pre_execution_host_observations_sha256: Sha256Digest,
        pre_execution_host_observations: Vec<CanaryHostObservation>,
        final_host_observations_sha256: Sha256Digest,
        final_host_observations: Vec<CanaryHostObservation>,
    ) -> Result<Self, ParallelProofError> {
        let receipt_sha256 = receipt.digest()?;
        let evidence = Self {
            schema_version: DRIVER_SCHEMA_VERSION,
            receipt,
            receipt_sha256,
            pre_execution_host_observations_sha256,
            pre_execution_host_observations,
            final_host_observations_sha256,
            final_host_observations,
            interrupted_transfer,
            model_calls: 0,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    /// Validate receipt integrity plus interruption/byte relationships.
    pub fn validate(&self) -> Result<(), ParallelProofError> {
        self.receipt.validate()?;
        if self.schema_version != DRIVER_SCHEMA_VERSION
            || self.model_calls != 0
            || self.receipt.model_calls != 0
            || self.receipt.digest()? != self.receipt_sha256
        {
            return Err(ParallelProofError::CorruptRecord(
                "Pulp macOS canary evidence identity".to_owned(),
            ));
        }
        let pre = evidence_hosts(&self.receipt, &self.pre_execution_host_observations)?;
        let final_observations = evidence_hosts(&self.receipt, &self.final_host_observations)?;
        if canary_host_observations_digest(pre.0, pre.1)?
            != self.pre_execution_host_observations_sha256
            || canary_host_observations_digest(final_observations.0, final_observations.1)?
                != self.final_host_observations_sha256
        {
            return Err(ParallelProofError::CorruptRecord(
                "Pulp macOS canary fence observations".to_owned(),
            ));
        }
        validate_interruption(&self.receipt, self.interrupted_transfer.as_ref())
    }

    /// Domain-separated digest of the complete evidence record.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        self.validate()?;
        domain_digest("shipyard.pulp-mac-canary.evidence.v1", self)
    }
}

/// Immutable lifecycle state for a potentially side-effecting canary attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PulpMacCanaryAttemptState {
    /// Durable intent exists before transfer or distributed execution begins.
    DistributedStarted,
    /// A post-start step failed; autonomous retry is forbidden.
    Failed,
}

/// Bounded immutable transition evidence for physical-work attempts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PulpMacCanaryAttemptRecord {
    /// Transition schema.
    pub schema_version: u32,
    /// Exact correlation identity.
    pub correlation_id: String,
    /// Exact admitted proof manifest.
    pub manifest_digest: Sha256Digest,
    /// Attempt lifecycle state.
    pub state: PulpMacCanaryAttemptState,
    /// Digest of the authenticated control receipt.
    pub control_receipt_sha256: Sha256Digest,
    /// Digest of the complete pre-execution host observations.
    pub pre_execution_host_observations_sha256: Sha256Digest,
    /// Domain-separated digest of the terminal error, only for failed state.
    pub failure_sha256: Option<Sha256Digest>,
}

/// Result of one controller invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PulpMacCanaryDriverOutcome {
    /// Policy is disabled; no adapter method was called.
    Disabled,
    /// Admission refused the exact observations; no execution method was called.
    Ineligible(PulpMacCanaryDecision),
    /// Exact evidence was durably published or replayed byte-identically.
    Recorded {
        /// Immutable typed evidence.
        evidence: Box<PulpMacCanaryEvidence>,
        /// Durable no-overwrite publication result.
        write_outcome: StoreWriteOutcome,
    },
}

/// Crash-durable, no-overwrite store for canary measurement evidence.
#[derive(Clone, Debug)]
pub struct PulpMacCanaryEvidenceStore {
    store: ImmutableByteStore,
}

impl PulpMacCanaryEvidenceStore {
    /// Create or reopen a controller-owned evidence root.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ParallelProofError> {
        Ok(Self {
            store: ImmutableByteStore::open(root, MAX_RECORD_BYTES).map_err(map_store_error)?,
        })
    }

    /// Publish evidence under its correlation id without overwriting conflicts.
    pub fn record(
        &self,
        evidence: &PulpMacCanaryEvidence,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        evidence.validate()?;
        self.store
            .put(
                &evidence.receipt.correlation_id,
                &serde_json::to_vec(evidence)?,
            )
            .map_err(map_store_error)
    }

    /// Load and integrity-check evidence by exact correlation id.
    pub fn load(&self, correlation_id: &str) -> Result<PulpMacCanaryEvidence, ParallelProofError> {
        let evidence: PulpMacCanaryEvidence =
            serde_json::from_slice(&self.store.load(correlation_id).map_err(map_store_error)?)?;
        evidence.validate()?;
        if evidence.receipt.correlation_id != correlation_id {
            return Err(ParallelProofError::CorruptRecord(
                "canary evidence logical key mismatch".to_owned(),
            ));
        }
        Ok(evidence)
    }

    fn record_attempt(
        &self,
        record: &PulpMacCanaryAttemptRecord,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        if record.schema_version != DRIVER_SCHEMA_VERSION
            || matches!(record.state, PulpMacCanaryAttemptState::DistributedStarted)
                != record.failure_sha256.is_none()
        {
            return Err(ParallelProofError::InvalidField("canary attempt record"));
        }
        self.publish_bytes(
            &format!("attempt-{}-{:?}", record.correlation_id, record.state),
            &serde_json::to_vec(record)?,
        )
    }

    fn attempt_exists(&self, correlation_id: &str) -> Result<bool, ParallelProofError> {
        for state in [
            PulpMacCanaryAttemptState::DistributedStarted,
            PulpMacCanaryAttemptState::Failed,
        ] {
            let key = format!("attempt-{correlation_id}-{state:?}");
            if self.store.contains(&key).map_err(map_store_error)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn publish_bytes(
        &self,
        logical_key: &str,
        bytes: &[u8],
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        self.store.put(logical_key, bytes).map_err(map_store_error)
    }
}

fn map_store_error(error: ImmutableStoreError) -> ParallelProofError {
    match error {
        ImmutableStoreError::InvalidRoot => {
            ParallelProofError::InvalidField("canary evidence root")
        }
        ImmutableStoreError::UnsafePath(path) => ParallelProofError::CorruptRecord(format!(
            "unsafe canary evidence path {}",
            path.display()
        )),
        ImmutableStoreError::LimitExceeded { max, found } => ParallelProofError::LimitExceeded {
            field: "canary evidence bytes",
            max,
            found,
        },
        ImmutableStoreError::Missing(key) => ParallelProofError::MissingRecord(key),
        ImmutableStoreError::Conflict(key) => ParallelProofError::ImmutableConflict(key),
        ImmutableStoreError::Io(error) => ParallelProofError::Io(error),
    }
}

/// Run an admitted canary sequentially and durably publish exact evidence.
#[allow(clippy::too_many_arguments)]
pub fn drive_pulp_mac_canary<E: PulpMacCanaryExecutor>(
    proof: ParallelProofContext<'_>,
    policy: &PulpMacCanaryPolicy,
    timing: &CanaryTimingEstimate,
    correlation_id: impl Into<String>,
    executor: &mut E,
    store: &PulpMacCanaryEvidenceStore,
) -> Result<PulpMacCanaryDriverOutcome, ParallelProofError> {
    let proof = ParallelProofContext::new(proof.manifest, proof.inventory, proof.plan)?;
    if !policy.enabled {
        return Ok(PulpMacCanaryDriverOutcome::Disabled);
    }
    let correlation_id = correlation_id.into();
    validate_correlation_id(&correlation_id)?;
    let manifest_digest = proof.manifest.digest(proof.inventory, proof.plan)?;
    match store.load(&correlation_id) {
        Ok(evidence)
            if evidence.receipt.manifest_digest == manifest_digest
                && evidence.receipt.validate_against(proof, policy).is_ok() =>
        {
            return Ok(PulpMacCanaryDriverOutcome::Recorded {
                evidence: Box::new(evidence),
                write_outcome: StoreWriteOutcome::AlreadyPresent,
            });
        }
        Ok(_) => return Err(ParallelProofError::ImmutableConflict(correlation_id)),
        Err(ParallelProofError::MissingRecord(_)) => {}
        Err(error) => return Err(error),
    }
    if store.attempt_exists(&correlation_id)? {
        return Err(ParallelProofError::InvalidAttemptSequence(format!(
            "canary attempt {correlation_id} requires reconciliation"
        )));
    }
    let initial_hosts = executor.authenticated_host_observations()?;
    let mut assessed_policy = policy.clone();
    assessed_policy.assessed_at_ms = executor.controller_now_ms()?;
    let decision = assess_pulp_mac_canary(proof, &assessed_policy, &initial_hosts, timing)?;
    if !matches!(decision, PulpMacCanaryDecision::Eligible { .. }) {
        return Ok(PulpMacCanaryDriverOutcome::Ineligible(decision));
    }
    let (builder, worker) = admitted_hosts(&assessed_policy, &initial_hosts)?;

    // The control must fully finish before any transfer or shard execution.
    let control_receipt = executor.run_single_host_control(proof, builder)?;
    control_receipt.validate(proof, &assessed_policy, builder)?;

    let pre_execution_hosts = executor.authenticated_host_observations()?;
    let pre_execution_now_ms = executor.controller_now_ms()?;
    let pre_execution_host_observations_sha256 = validate_host_fence(
        &assessed_policy,
        proof,
        builder,
        worker,
        &pre_execution_hosts,
        pre_execution_now_ms,
        StorageFence::BeforeTransfer,
    )?;
    let control_receipt_sha256 = control_receipt.digest(proof, &assessed_policy, builder)?;
    let claim_outcome = store.record_attempt(&PulpMacCanaryAttemptRecord {
        schema_version: DRIVER_SCHEMA_VERSION,
        correlation_id: correlation_id.clone(),
        manifest_digest: manifest_digest.clone(),
        state: PulpMacCanaryAttemptState::DistributedStarted,
        control_receipt_sha256: control_receipt_sha256.clone(),
        pre_execution_host_observations_sha256: pre_execution_host_observations_sha256.clone(),
        failure_sha256: None,
    })?;
    if claim_outcome != StoreWriteOutcome::Created {
        return Err(ParallelProofError::InvalidAttemptSequence(format!(
            "canary attempt {correlation_id} is already claimed"
        )));
    }

    let post_start = finish_started_canary(
        proof,
        &assessed_policy,
        &decision,
        &correlation_id,
        executor,
        store,
        &manifest_digest,
        builder,
        worker,
        control_receipt,
        pre_execution_host_observations_sha256.clone(),
        pre_execution_hosts,
    );
    match post_start {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            store.record_attempt(&PulpMacCanaryAttemptRecord {
                schema_version: DRIVER_SCHEMA_VERSION,
                correlation_id,
                manifest_digest,
                state: PulpMacCanaryAttemptState::Failed,
                control_receipt_sha256,
                pre_execution_host_observations_sha256,
                failure_sha256: Some(domain_digest(
                    "shipyard.pulp-mac-canary.failure.v1",
                    &error.to_string(),
                )?),
            })?;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_started_canary<E: PulpMacCanaryExecutor>(
    proof: ParallelProofContext<'_>,
    policy: &PulpMacCanaryPolicy,
    decision: &PulpMacCanaryDecision,
    correlation_id: &str,
    executor: &mut E,
    store: &PulpMacCanaryEvidenceStore,
    manifest_digest: &Sha256Digest,
    builder: &CanaryHostObservation,
    worker: &CanaryHostObservation,
    control_receipt: SingleHostControlReceipt,
    pre_execution_host_observations_sha256: Sha256Digest,
    pre_execution_hosts: Vec<CanaryHostObservation>,
) -> Result<PulpMacCanaryDriverOutcome, ParallelProofError> {
    let distributed = executor.run_distributed_shadow(manifest_digest)?;
    let final_hosts = executor.authenticated_host_observations()?;
    let final_now_ms = executor.controller_now_ms()?;
    let final_host_observations_sha256 = validate_host_fence(
        policy,
        proof,
        builder,
        worker,
        &final_hosts,
        final_now_ms,
        StorageFence::AfterTransfer,
    )?;
    validate_delivery_observation(proof, &distributed.delivery)?;
    let caches = distributed
        .caches
        .into_iter()
        .map(|cache| CanaryCacheMeasurement {
            generation: cache.generation,
            usage: cache.usage,
            claimed_bytes_avoided: 0,
        })
        .collect();
    let interruption = distributed.delivery.interruption.clone();
    let verified_resume_offset_bytes = match distributed.delivery.mode {
        ArtifactDeliveryMode::FullTransfer => 0,
        ArtifactDeliveryMode::VerifiedPrefixResume => {
            interruption
                .as_ref()
                .ok_or(ParallelProofError::InvalidField("canary delivery evidence"))?
                .verified_resume_offset_bytes
        }
        ArtifactDeliveryMode::ImmutableObjectReuse => distributed.delivery.artifact_bytes_total,
    };
    let input = CanaryMeasurementInput {
        correlation_id: correlation_id.to_owned(),
        delivery_mode: distributed.delivery.mode,
        artifact_bytes_total: distributed.delivery.artifact_bytes_total,
        artifact_bytes_reused: distributed.delivery.artifact_bytes_reused,
        artifact_bytes_transferred: distributed.delivery.artifact_bytes_transferred,
        verified_resume_offset_bytes,
        setup_ms: distributed.setup_ms,
        transfer_ms: distributed.transfer_ms,
        verification_ms: distributed.verification_ms,
        dispatch_ms: distributed.dispatch_ms,
        shard_execution_ms: distributed.shard_execution_ms,
        worker_active_ms: distributed.worker_active_ms,
        submit_to_receipt_ms: distributed.submit_to_receipt_ms,
        single_host_control: control_receipt,
        caches,
        model_calls: 0,
    };
    let receipt = PulpMacCanaryMeasurementReceipt::capture(
        proof, policy, decision, builder, worker, builder, input,
    )?;
    let evidence = PulpMacCanaryEvidence::new(
        receipt,
        interruption,
        pre_execution_host_observations_sha256,
        pre_execution_hosts,
        final_host_observations_sha256,
        final_hosts,
    )?;
    let write_outcome = store.record(&evidence)?;
    Ok(PulpMacCanaryDriverOutcome::Recorded {
        evidence: Box::new(evidence),
        write_outcome,
    })
}

fn evidence_hosts<'a>(
    receipt: &PulpMacCanaryMeasurementReceipt,
    hosts: &'a [CanaryHostObservation],
) -> Result<(&'a CanaryHostObservation, &'a CanaryHostObservation), ParallelProofError> {
    if hosts.len() != 2 {
        return Err(ParallelProofError::CorruptRecord(
            "canary fence host count".to_owned(),
        ));
    }
    let builder = hosts
        .iter()
        .find(|host| host.host_id == receipt.builder_host_id);
    let worker = hosts
        .iter()
        .find(|host| host.host_id == receipt.worker_host_id);
    match (builder, worker) {
        (Some(builder), Some(worker))
            if builder.session_generation == receipt.builder_session_generation
                && worker.session_generation == receipt.worker_session_generation =>
        {
            Ok((builder, worker))
        }
        _ => Err(ParallelProofError::CorruptRecord(
            "canary fence host identity".to_owned(),
        )),
    }
}

fn admitted_hosts<'a>(
    policy: &PulpMacCanaryPolicy,
    hosts: &'a [CanaryHostObservation],
) -> Result<(&'a CanaryHostObservation, &'a CanaryHostObservation), ParallelProofError> {
    let unique = |id: &str| {
        let mut matches = hosts.iter().filter(|host| host.host_id == id);
        let host = matches.next();
        (host, matches.next())
    };
    let (builder, duplicate_builder) = unique(&policy.builder_host_id);
    let (worker, duplicate_worker) = unique(&policy.worker_host_id);
    match (builder, worker, duplicate_builder, duplicate_worker) {
        (Some(builder), Some(worker), None, None) => Ok((builder, worker)),
        _ => Err(ParallelProofError::BindingMismatch("canary admitted hosts")),
    }
}

fn validate_host_fence(
    policy: &PulpMacCanaryPolicy,
    proof: ParallelProofContext<'_>,
    admitted_builder: &CanaryHostObservation,
    admitted_worker: &CanaryHostObservation,
    observed: &[CanaryHostObservation],
    controller_now_ms: u64,
    storage_fence: StorageFence,
) -> Result<Sha256Digest, ParallelProofError> {
    let (builder, worker) = admitted_hosts(policy, observed)?;
    if builder.host_id != admitted_builder.host_id
        || builder.session_generation != admitted_builder.session_generation
        || builder.route != admitted_builder.route
        || builder.staging_root != admitted_builder.staging_root
        || builder.staging_class != admitted_builder.staging_class
        || builder.capabilities != admitted_builder.capabilities
        || builder.cache_generations != admitted_builder.cache_generations
        || worker.host_id != admitted_worker.host_id
        || worker.session_generation != admitted_worker.session_generation
        || worker.route != admitted_worker.route
        || worker.staging_root != admitted_worker.staging_root
        || worker.staging_class != admitted_worker.staging_class
        || worker.capabilities != admitted_worker.capabilities
        || worker.cache_generations != admitted_worker.cache_generations
        || !builder.online
        || !worker.online
        || controller_now_ms == 0
        || policy.maximum_observation_age_ms == 0
        || builder.observed_at_ms < admitted_builder.observed_at_ms
        || worker.observed_at_ms < admitted_worker.observed_at_ms
        || builder.observed_at_ms > controller_now_ms
        || worker.observed_at_ms > controller_now_ms
        || controller_now_ms.saturating_sub(builder.observed_at_ms)
            > policy.maximum_observation_age_ms
        || controller_now_ms.saturating_sub(worker.observed_at_ms)
            > policy.maximum_observation_age_ms
    {
        return Err(ParallelProofError::BindingMismatch("canary host fence"));
    }
    let required_free = match storage_fence {
        StorageFence::BeforeTransfer => policy
            .minimum_free_bytes
            .checked_add(proof.manifest.artifact.size_bytes)
            .ok_or(ParallelProofError::InvalidField("canary storage reserve"))?,
        StorageFence::AfterTransfer => policy.minimum_free_bytes,
    };
    if builder.free_bytes < required_free || worker.free_bytes < required_free {
        return Err(ParallelProofError::BindingMismatch(
            "canary storage reserve",
        ));
    }
    canary_host_observations_digest(builder, worker)
}

#[derive(Clone, Copy)]
enum StorageFence {
    BeforeTransfer,
    AfterTransfer,
}

fn validate_delivery_observation(
    proof: ParallelProofContext<'_>,
    delivery: &ArtifactDeliveryObservation,
) -> Result<(), ParallelProofError> {
    if delivery.artifact_bytes_total != proof.manifest.artifact.size_bytes
        || delivery
            .artifact_bytes_reused
            .checked_add(delivery.artifact_bytes_transferred)
            != Some(delivery.artifact_bytes_total)
    {
        return Err(ParallelProofError::BindingMismatch(
            "canary transport counters",
        ));
    }
    match (delivery.mode, delivery.interruption.as_ref()) {
        (ArtifactDeliveryMode::VerifiedPrefixResume, Some(interruption))
            if interruption.verified_resume_offset_bytes == delivery.artifact_bytes_reused
                && interruption.bytes_before_interruption
                    >= interruption.verified_resume_offset_bytes
                && interruption.bytes_after_resume == delivery.artifact_bytes_transferred
                && interruption.verified_resume_offset_bytes > 0
                && interruption.verified_resume_offset_bytes < delivery.artifact_bytes_total => {}
        (ArtifactDeliveryMode::FullTransfer, None)
            if delivery.artifact_bytes_reused == 0
                && delivery.artifact_bytes_transferred == delivery.artifact_bytes_total => {}
        (ArtifactDeliveryMode::ImmutableObjectReuse, None)
            if delivery.artifact_bytes_reused == delivery.artifact_bytes_total
                && delivery.artifact_bytes_transferred == 0 => {}
        _ => return Err(ParallelProofError::InvalidField("canary delivery evidence")),
    }
    Ok(())
}

fn validate_interruption(
    receipt: &PulpMacCanaryMeasurementReceipt,
    interruption: Option<&InterruptedTransferEvidence>,
) -> Result<(), ParallelProofError> {
    match (receipt.delivery_mode, interruption) {
        (ArtifactDeliveryMode::VerifiedPrefixResume, Some(interruption))
            if interruption.verified_resume_offset_bytes
                == receipt.verified_resume_offset_bytes
                && interruption.bytes_after_resume == receipt.artifact_bytes_transferred
                && interruption.bytes_before_interruption
                    >= interruption.verified_resume_offset_bytes =>
        {
            Ok(())
        }
        (ArtifactDeliveryMode::FullTransfer | ArtifactDeliveryMode::ImmutableObjectReuse, None) => {
            Ok(())
        }
        _ => Err(ParallelProofError::CorruptRecord(
            "canary interruption evidence".to_owned(),
        )),
    }
}

fn domain_digest(domain: &str, value: &impl Serialize) -> Result<Sha256Digest, ParallelProofError> {
    let payload = serde_json::to_vec(value)?;
    let mut bytes = Vec::with_capacity(16 + domain.len() + payload.len());
    bytes.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    bytes.extend_from_slice(domain.as_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    bytes.extend_from_slice(&payload);
    Ok(Sha256Digest::of_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parallel_proof::{
        ArtifactIdentity, ArtifactTrustClass, BuildIdentity, ExecutionBoundary,
        ParallelProofManifest, ProofSubject, ShardPlan, SourceIdentity, TestCase, TestInventory,
        TrustIdentity,
    };
    use crate::parallel_proof_canary::{CanaryRoute, CanaryStagingClass};

    struct Fixture {
        inventory: TestInventory,
        plan: ShardPlan,
        manifest: ParallelProofManifest,
    }

    impl Fixture {
        fn proof(&self) -> ParallelProofContext<'_> {
            ParallelProofContext::new(&self.manifest, &self.inventory, &self.plan).unwrap()
        }
    }

    fn digest(value: &str) -> Sha256Digest {
        Sha256Digest::of_bytes(value.as_bytes())
    }

    fn fixture() -> Fixture {
        let inventory = TestInventory::new(vec![
            TestCase {
                id: "audio".to_owned(),
                dependencies: vec![],
                fixture_setup: vec![],
                fixture_required: vec![],
                fixture_cleanup: vec![],
                run_serial: false,
                resource_locks: vec![],
                required_capabilities: vec!["macos-arm64".to_owned()],
            },
            TestCase {
                id: "dsp".to_owned(),
                dependencies: vec![],
                fixture_setup: vec![],
                fixture_required: vec![],
                fixture_cleanup: vec![],
                run_serial: false,
                resource_locks: vec![],
                required_capabilities: vec!["macos-arm64".to_owned()],
            },
        ])
        .unwrap();
        let plan = ShardPlan::deterministic_balanced(&inventory, 2).unwrap();
        let tree_sha = "b".repeat(64);
        let build = BuildIdentity {
            contract_sha256: digest("contract"),
            toolchain_sha256: digest("toolchain"),
            target_triple: "aarch64-apple-darwin".to_owned(),
            profile: "release".to_owned(),
        };
        let manifest = ParallelProofManifest::new(
            SourceIdentity {
                repository_id: 1_203_111_607,
                repository: "generous-corp/pulp".to_owned(),
                subject: ProofSubject::PullRequest { number: 88 },
                head_sha: "a".repeat(64),
                tree_sha: tree_sha.clone(),
            },
            build.clone(),
            ArtifactIdentity {
                source_tree_sha: tree_sha,
                build_contract_sha256: build.contract_sha256,
                payload_sha256: digest("artifact"),
                layout_sha256: digest("layout"),
                size_bytes: 1_000,
            },
            TrustIdentity {
                producer_identity_sha256: digest("producer"),
                image_sha256: digest("image"),
                policy_sha256: digest("policy"),
                artifact_class: ArtifactTrustClass::TrustedController,
                execution_boundary: ExecutionBoundary::TrustedHost,
                network_enabled: false,
                writable_host_mounts: false,
            },
            &inventory,
            &plan,
        )
        .unwrap();
        Fixture {
            inventory,
            plan,
            manifest,
        }
    }

    fn different_fixture() -> Fixture {
        let mut different = fixture();
        let mut source = different.manifest.source.clone();
        source.head_sha = "c".repeat(64);
        different.manifest = ParallelProofManifest::new(
            source,
            different.manifest.build.clone(),
            different.manifest.artifact.clone(),
            different.manifest.trust.clone(),
            &different.inventory,
            &different.plan,
        )
        .unwrap();
        different
    }

    fn cache() -> CanaryCacheGeneration {
        CanaryCacheGeneration {
            name: "skia".to_owned(),
            generation: "m124-arm64".to_owned(),
            sha256: digest("skia"),
        }
    }

    fn hosts(free_bytes: u64) -> Vec<CanaryHostObservation> {
        vec![
            CanaryHostObservation {
                host_id: "m3".to_owned(),
                online: true,
                observed_at_ms: 9_500,
                session_generation: 11,
                route: CanaryRoute::SameHost,
                staging_root: "/var/lib/shipyard/m3".to_owned(),
                staging_class: CanaryStagingClass::Persistent,
                free_bytes,
                capabilities: vec!["macos-arm64".to_owned()],
                cache_generations: vec![cache()],
            },
            CanaryHostObservation {
                host_id: "m1".to_owned(),
                online: true,
                observed_at_ms: 9_500,
                session_generation: 22,
                route: CanaryRoute::Lan,
                staging_root: "/var/lib/shipyard/m1".to_owned(),
                staging_class: CanaryStagingClass::Persistent,
                free_bytes,
                capabilities: vec!["macos-arm64".to_owned()],
                cache_generations: vec![cache()],
            },
        ]
    }

    fn policy(enabled: bool) -> PulpMacCanaryPolicy {
        PulpMacCanaryPolicy {
            enabled,
            repository_id: 1_203_111_607,
            repository: "generous-corp/pulp".to_owned(),
            target: "mac".to_owned(),
            target_triple: "aarch64-apple-darwin".to_owned(),
            builder_host_id: "m3".to_owned(),
            worker_host_id: "m1".to_owned(),
            assessed_at_ms: 10_000,
            maximum_observation_age_ms: 1_000,
            minimum_free_bytes: 500,
            required_cache_generations: vec![cache()],
        }
    }

    fn timing(fixture: &Fixture) -> CanaryTimingEstimate {
        CanaryTimingEstimate {
            manifest_digest: fixture
                .manifest
                .digest(&fixture.inventory, &fixture.plan)
                .unwrap(),
            target: "mac".to_owned(),
            single_host_ms: 1_000_000,
            distributed_shard_ms: 600_000,
            transfer_and_dispatch_ms: 30_000,
        }
    }

    fn distributed() -> DistributedExecutionObservation {
        DistributedExecutionObservation {
            delivery: ArtifactDeliveryObservation {
                mode: ArtifactDeliveryMode::VerifiedPrefixResume,
                artifact_bytes_total: 1_000,
                artifact_bytes_reused: 400,
                artifact_bytes_transferred: 600,
                interruption: Some(InterruptedTransferEvidence {
                    interrupted_partial_sha256: digest("partial"),
                    verified_prefix_sha256: digest("prefix"),
                    bytes_before_interruption: 500,
                    verified_resume_offset_bytes: 400,
                    bytes_after_resume: 600,
                }),
            },
            setup_ms: 1_000,
            transfer_ms: 2_000,
            verification_ms: 1_000,
            dispatch_ms: 1_000,
            shard_execution_ms: 500_000,
            worker_active_ms: 800_000,
            submit_to_receipt_ms: 505_000,
            caches: vec![ObservedCacheUse {
                generation: cache(),
                usage: CacheUse::Hit,
            }],
        }
    }

    struct FakeExecutor {
        observations: Vec<Vec<CanaryHostObservation>>,
        calls: Vec<&'static str>,
        distributed: DistributedExecutionObservation,
        corrupt_control: bool,
    }

    impl FakeExecutor {
        fn normal() -> Self {
            Self {
                observations: vec![hosts(10_000), hosts(10_000), hosts(10_000)],
                calls: vec![],
                distributed: distributed(),
                corrupt_control: false,
            }
        }
    }

    impl PulpMacCanaryExecutor for FakeExecutor {
        fn controller_now_ms(&mut self) -> Result<u64, ParallelProofError> {
            self.calls.push("now");
            Ok(10_000)
        }

        fn authenticated_host_observations(
            &mut self,
        ) -> Result<Vec<CanaryHostObservation>, ParallelProofError> {
            self.calls.push("observe");
            if self.observations.is_empty() {
                return Err(ParallelProofError::InvalidField("test observations"));
            }
            Ok(self.observations.remove(0))
        }

        fn run_single_host_control(
            &mut self,
            proof: ParallelProofContext<'_>,
            host: &CanaryHostObservation,
        ) -> Result<SingleHostControlReceipt, ParallelProofError> {
            self.calls.push("control");
            let mut receipt =
                SingleHostControlReceipt::capture(proof, &policy(true), host, 800_000, 790_000, 0)?;
            if self.corrupt_control {
                receipt.artifact_sha256 = digest("wrong-artifact");
            }
            Ok(receipt)
        }

        fn run_distributed_shadow(
            &mut self,
            _manifest_digest: &Sha256Digest,
        ) -> Result<DistributedExecutionObservation, ParallelProofError> {
            self.calls.push("distributed");
            Ok(self.distributed.clone())
        }
    }

    fn store(directory: &tempfile::TempDir) -> PulpMacCanaryEvidenceStore {
        PulpMacCanaryEvidenceStore::open(directory.path().join("canary")).unwrap()
    }

    #[test]
    fn disabled_policy_never_calls_executor() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let mut executor = FakeExecutor::normal();
        let outcome = drive_pulp_mac_canary(
            fixture.proof(),
            &policy(false),
            &timing(&fixture),
            "disabled",
            &mut executor,
            &store(&temporary),
        )
        .unwrap();
        assert_eq!(outcome, PulpMacCanaryDriverOutcome::Disabled);
        assert!(executor.calls.is_empty());
    }

    #[test]
    fn ineligible_observation_never_executes_work() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let mut executor = FakeExecutor::normal();
        executor.observations[0][1].online = false;
        let outcome = drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "offline",
            &mut executor,
            &store(&temporary),
        )
        .unwrap();
        assert!(matches!(outcome, PulpMacCanaryDriverOutcome::Ineligible(_)));
        assert_eq!(executor.calls, vec!["observe", "now"]);
    }

    #[test]
    fn executes_control_before_transfer_and_records_actual_resume_counters() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let evidence_store = store(&temporary);
        let mut executor = FakeExecutor::normal();
        let outcome = drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "resume-1",
            &mut executor,
            &evidence_store,
        )
        .unwrap();
        assert_eq!(
            executor.calls,
            vec![
                "observe",
                "now",
                "control",
                "observe",
                "now",
                "distributed",
                "observe",
                "now"
            ]
        );
        let PulpMacCanaryDriverOutcome::Recorded {
            evidence,
            write_outcome,
        } = outcome
        else {
            panic!("expected evidence");
        };
        assert_eq!(write_outcome, StoreWriteOutcome::Created);
        assert_eq!(evidence.receipt.artifact_bytes_reused, 400);
        assert_eq!(evidence.receipt.artifact_bytes_transferred, 600);
        assert_eq!(evidence.receipt.model_calls, 0);
        assert_eq!(evidence.receipt.caches[0].claimed_bytes_avoided, 0);
        let mut tampered_fence = (*evidence).clone();
        tampered_fence.final_host_observations[1].free_bytes -= 1;
        assert!(matches!(
            tampered_fence.validate(),
            Err(ParallelProofError::CorruptRecord(_))
        ));
        assert_eq!(
            evidence
                .interrupted_transfer
                .unwrap()
                .bytes_before_interruption,
            500
        );
        assert_eq!(
            evidence_store.load("resume-1").unwrap(),
            evidence_store.load("resume-1").unwrap()
        );
    }

    #[test]
    fn immutable_replay_is_idempotent_without_reexecution_and_conflicts_fail_closed() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let evidence_store = store(&temporary);
        let mut first = FakeExecutor::normal();
        let first_outcome = drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "same-key",
            &mut first,
            &evidence_store,
        )
        .unwrap();
        assert!(matches!(
            first_outcome,
            PulpMacCanaryDriverOutcome::Recorded {
                write_outcome: StoreWriteOutcome::Created,
                ..
            }
        ));
        let mut replay = FakeExecutor::normal();
        let replay_outcome = drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "same-key",
            &mut replay,
            &evidence_store,
        )
        .unwrap();
        assert!(matches!(
            replay_outcome,
            PulpMacCanaryDriverOutcome::Recorded {
                write_outcome: StoreWriteOutcome::AlreadyPresent,
                ..
            }
        ));
        assert!(replay.calls.is_empty());
        let other = different_fixture();
        let mut wrong_proof_replay = FakeExecutor::normal();
        assert!(matches!(
            drive_pulp_mac_canary(
                other.proof(),
                &policy(true),
                &timing(&other),
                "same-key",
                &mut wrong_proof_replay,
                &evidence_store,
            ),
            Err(ParallelProofError::ImmutableConflict(_))
        ));
        assert!(wrong_proof_replay.calls.is_empty());

        let mut wrong_scope = policy(true);
        wrong_scope.target = "release-mac".to_owned();
        let mut wrong_scope_replay = FakeExecutor::normal();
        assert!(matches!(
            drive_pulp_mac_canary(
                fixture.proof(),
                &wrong_scope,
                &timing(&fixture),
                "same-key",
                &mut wrong_scope_replay,
                &evidence_store,
            ),
            Err(ParallelProofError::ImmutableConflict(_))
        ));
        assert!(wrong_scope_replay.calls.is_empty());

        let conflict_directory = tempfile::tempdir().unwrap();
        let conflict_store = store(&conflict_directory);
        let mut conflict = FakeExecutor::normal();
        conflict.distributed.worker_active_ms += 1;
        let PulpMacCanaryDriverOutcome::Recorded {
            evidence: conflicting_evidence,
            ..
        } = drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "same-key",
            &mut conflict,
            &conflict_store,
        )
        .unwrap()
        else {
            panic!("expected conflicting evidence");
        };
        assert!(matches!(
            evidence_store.record(&conflicting_evidence),
            Err(ParallelProofError::ImmutableConflict(_))
        ));
    }

    #[test]
    fn session_change_before_transfer_fails_closed() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let mut executor = FakeExecutor::normal();
        executor.observations[1][1].session_generation += 1;
        assert!(matches!(
            drive_pulp_mac_canary(
                fixture.proof(),
                &policy(true),
                &timing(&fixture),
                "session-change",
                &mut executor,
                &store(&temporary),
            ),
            Err(ParallelProofError::BindingMismatch("canary host fence"))
        ));
        assert_eq!(
            executor.calls,
            vec!["observe", "now", "control", "observe", "now"]
        );
    }

    #[test]
    fn control_receipt_must_bind_exact_proof_and_session() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let mut executor = FakeExecutor::normal();
        executor.corrupt_control = true;
        assert!(matches!(
            drive_pulp_mac_canary(
                fixture.proof(),
                &policy(true),
                &timing(&fixture),
                "wrong-control",
                &mut executor,
                &store(&temporary),
            ),
            Err(ParallelProofError::BindingMismatch(
                "single-host control receipt"
            ))
        ));
        assert_eq!(executor.calls, vec!["observe", "now", "control"]);
    }

    #[test]
    fn reserve_loss_after_execution_blocks_publication() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let mut executor = FakeExecutor::normal();
        executor.observations[2] = hosts(499);
        assert!(matches!(
            drive_pulp_mac_canary(
                fixture.proof(),
                &policy(true),
                &timing(&fixture),
                "reserve-loss",
                &mut executor,
                &store(&temporary),
            ),
            Err(ParallelProofError::BindingMismatch(
                "canary storage reserve"
            ))
        ));
    }

    #[test]
    fn stale_execution_fence_is_rejected_after_control() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let mut executor = FakeExecutor::normal();
        executor.observations[1][0].observed_at_ms = 8_999;
        executor.observations[1][1].observed_at_ms = 8_999;
        assert!(matches!(
            drive_pulp_mac_canary(
                fixture.proof(),
                &policy(true),
                &timing(&fixture),
                "stale-fence",
                &mut executor,
                &store(&temporary),
            ),
            Err(ParallelProofError::BindingMismatch("canary host fence"))
        ));
        assert_eq!(
            executor.calls,
            vec!["observe", "now", "control", "observe", "now"]
        );
    }

    #[test]
    fn forged_resume_counters_are_rejected() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let mut executor = FakeExecutor::normal();
        executor
            .distributed
            .delivery
            .interruption
            .as_mut()
            .unwrap()
            .bytes_after_resume = 599;
        let evidence_store = store(&temporary);
        assert!(matches!(
            drive_pulp_mac_canary(
                fixture.proof(),
                &policy(true),
                &timing(&fixture),
                "bad-resume",
                &mut executor,
                &evidence_store,
            ),
            Err(ParallelProofError::InvalidField("canary delivery evidence"))
        ));
        let mut retry = FakeExecutor::normal();
        assert!(matches!(
            drive_pulp_mac_canary(
                fixture.proof(),
                &policy(true),
                &timing(&fixture),
                "bad-resume",
                &mut retry,
                &evidence_store,
            ),
            Err(ParallelProofError::InvalidAttemptSequence(_))
        ));
        assert!(retry.calls.is_empty());
    }

    #[test]
    fn full_transfer_and_immutable_reuse_derive_exact_offsets() {
        let fixture = fixture();
        for (correlation, mode, reused, transferred, expected_offset) in [
            ("full", ArtifactDeliveryMode::FullTransfer, 0, 1_000, 0),
            (
                "reuse",
                ArtifactDeliveryMode::ImmutableObjectReuse,
                1_000,
                0,
                1_000,
            ),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let mut executor = FakeExecutor::normal();
            executor.distributed.delivery.mode = mode;
            executor.distributed.delivery.artifact_bytes_reused = reused;
            executor.distributed.delivery.artifact_bytes_transferred = transferred;
            executor.distributed.delivery.interruption = None;
            let outcome = drive_pulp_mac_canary(
                fixture.proof(),
                &policy(true),
                &timing(&fixture),
                correlation,
                &mut executor,
                &store(&temporary),
            )
            .unwrap();
            let PulpMacCanaryDriverOutcome::Recorded { evidence, .. } = outcome else {
                panic!("expected recorded evidence");
            };
            assert_eq!(
                evidence.receipt.verified_resume_offset_bytes,
                expected_offset
            );
        }
    }

    #[test]
    fn cache_generation_must_match_authenticated_observation() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let mut executor = FakeExecutor::normal();
        executor.distributed.caches[0].generation.generation = "forged".to_owned();
        assert!(
            drive_pulp_mac_canary(
                fixture.proof(),
                &policy(true),
                &timing(&fixture),
                "bad-cache",
                &mut executor,
                &store(&temporary),
            )
            .is_err()
        );
    }

    #[test]
    fn public_measurement_types_do_not_expose_claimed_avoided_bytes() {
        let observed = ObservedCacheUse {
            generation: cache(),
            usage: CacheUse::Hit,
        };
        assert_eq!(observed.usage, CacheUse::Hit);
    }

    #[test]
    fn malformed_correlation_id_is_rejected_before_executor_calls() {
        let fixture = fixture();
        let temporary = tempfile::tempdir().unwrap();
        let mut executor = FakeExecutor::normal();
        assert!(matches!(
            drive_pulp_mac_canary(
                fixture.proof(),
                &policy(true),
                &timing(&fixture),
                "bad correlation",
                &mut executor,
                &store(&temporary),
            ),
            Err(ParallelProofError::InvalidField(
                "canary measurement correlation id"
            ))
        ));
        assert!(executor.calls.is_empty());
    }
}
