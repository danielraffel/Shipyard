//! Restart-reconcilable custody for protected parallel-proof canary jobs.
//!
//! This module deliberately does not accept a command line or shell text. An
//! authenticated controller submits one typed operation, persists intent before
//! launch, and delegates process mechanics to an adapter. After a controller or
//! agent restart, reconciliation observes the original launch nonce and process
//! identity; it never redispatches the operation. A missing process is a terminal
//! loss, never inferred success.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::immutable_store::{ImmutableByteStore, ImmutableStoreError};
use crate::parallel_proof::{ParallelProofError, Sha256Digest, StoreWriteOutcome};
#[cfg(test)]
use crate::parallel_proof_canary_driver::ArtifactDeliveryObservation;
use crate::parallel_proof_canary_driver::DistributedExecutionObservation;
use crate::parallel_proof_canary_receipt::ArtifactDeliveryMode;

const SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_ID_BYTES: usize = 160;
const MAX_HEARTBEATS: u32 = 128;
const MAX_LOG_SEGMENTS: u32 = 32;
const MAX_LOG_SEGMENT_BYTES: u32 = 256 * 1024;

/// The only side-effecting operation admitted by this controller.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovedCanaryOperation {
    /// Run the distributed shadow leg of an already-admitted parallel proof.
    ParallelProofDistributedShadow {
        /// Immutable GitHub repository database identifier.
        repository_id: u64,
        /// Explicit canonical owner/name. It is never inferred from cwd or a client.
        repository: String,
        /// Exact protected target selected by the admitted proof plan.
        target: String,
        /// Exact compilation/runtime triple selected by the proof plan.
        target_triple: String,
        /// Authenticated builder host identity.
        builder_host_id: String,
        /// Authenticated worker host identity.
        worker_host_id: String,
        /// Digest of the complete proof manifest, inventory, and plan.
        manifest_sha256: Sha256Digest,
        /// Exact manifest-bound encoded artifact size.
        artifact_bytes_total: u64,
        /// Digest of trusted machine-global invocation authority.
        invocation_authority_sha256: Sha256Digest,
        /// Digest of the exact provider adapter executable.
        adapter_executable_sha256: Sha256Digest,
    },
}

impl ApprovedCanaryOperation {
    fn validate(&self) -> Result<(), ParallelProofError> {
        let Self::ParallelProofDistributedShadow {
            repository_id,
            repository,
            target,
            target_triple,
            builder_host_id,
            worker_host_id,
            artifact_bytes_total,
            ..
        } = self;
        if *repository_id == 0
            || repository.matches('/').count() != 1
            || repository.starts_with('/')
            || repository.ends_with('/')
        {
            return Err(ParallelProofError::InvalidField("canary job repository"));
        }
        validate_id(repository, "canary job repository")?;
        validate_id(target, "canary job target")?;
        validate_id(target_triple, "canary job target triple")?;
        validate_id(builder_host_id, "canary job builder host")?;
        validate_id(worker_host_id, "canary job worker host")?;
        if builder_host_id == worker_host_id || *artifact_bytes_total == 0 {
            return Err(ParallelProofError::InvalidField(
                "canary job distinct hosts",
            ));
        }
        Ok(())
    }

    fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        self.validate()?;
        domain_digest("shipyard.canary-job.operation.v1", self)
    }
}

/// Authenticated logical owner; terminal adapters and model sessions are not owners.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryJobOwner {
    /// Stable controller identity.
    pub controller_id: String,
    /// Controller incarnation that approved this exact job.
    pub controller_incarnation: String,
    /// Digest of the authenticated approval record.
    pub approval_sha256: Sha256Digest,
}

/// Immutable success requirements for an adapter response artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanarySuccessPredicate {
    /// Only this exit code can be considered successful.
    pub required_exit_code: i32,
    /// Response schema required from the typed adapter.
    pub artifact_schema_version: u32,
    /// Maximum accepted response size.
    pub max_artifact_bytes: u32,
}

/// Immutable bounded cancellation policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryCancellationPolicy {
    /// Grace period the backend may spend proving complete-tree termination.
    pub grace_ms: u64,
    /// Cancel automatically at the job deadline.
    pub cancel_at_deadline: bool,
}

/// Immutable terminal wake policy. It contains no prompt or provider route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryWakePredicate {
    /// Emit one receipt wake after success.
    pub on_success: bool,
    /// Emit one actionable wake after failure, loss, or uncertain cancellation.
    pub on_actionable_failure: bool,
}

/// Bounded redacted log segmentation policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryLogPolicy {
    /// Maximum bytes retained in one immutable segment.
    pub segment_bytes: u32,
    /// Maximum immutable rotated segments retained for the job.
    pub max_segments: u32,
}

/// Immutable controller-approved job envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedCanaryJob {
    /// Envelope schema.
    pub schema_version: u32,
    /// Stable exact job identity.
    pub job_id: String,
    /// Existing canary correlation identity.
    pub correlation_id: String,
    /// Authenticated logical owner.
    pub owner: CanaryJobOwner,
    /// Closed operation vocabulary; never a command string.
    pub operation: ApprovedCanaryOperation,
    /// Controller time at approval.
    pub approved_at_ms: u64,
    /// Absolute controller deadline.
    pub deadline_at_ms: u64,
    /// Expected heartbeat interval.
    pub heartbeat_interval_ms: u64,
    /// Silence after which the process must be reconciled or cancelled.
    pub heartbeat_timeout_ms: u64,
    /// Hard bound on immutable heartbeat receipts.
    pub max_heartbeat_receipts: u32,
    /// Exact response success predicate.
    pub success: CanarySuccessPredicate,
    /// Complete-tree cancellation policy.
    pub cancellation: CanaryCancellationPolicy,
    /// Terminal wake classification.
    pub wake: CanaryWakePredicate,
    /// Redacted rotated log bounds.
    pub logs: CanaryLogPolicy,
}

impl ApprovedCanaryJob {
    /// Validate every bound and timing relationship.
    pub fn validate(&self) -> Result<(), ParallelProofError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ParallelProofError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        validate_id(&self.job_id, "canary job id")?;
        validate_id(&self.correlation_id, "canary correlation id")?;
        validate_id(&self.owner.controller_id, "canary controller id")?;
        validate_id(
            &self.owner.controller_incarnation,
            "canary controller incarnation",
        )?;
        if self.approved_at_ms == 0
            || self.deadline_at_ms <= self.approved_at_ms
            || self.heartbeat_interval_ms == 0
            || self.heartbeat_timeout_ms < self.heartbeat_interval_ms
            || self.max_heartbeat_receipts == 0
            || self.max_heartbeat_receipts > MAX_HEARTBEATS
            || self.success.artifact_schema_version == 0
            || self.success.max_artifact_bytes == 0
            || self.success.max_artifact_bytes as usize > MAX_RECORD_BYTES
            || self.logs.segment_bytes == 0
            || self.logs.segment_bytes > MAX_LOG_SEGMENT_BYTES
            || self.logs.max_segments == 0
            || self.logs.max_segments > MAX_LOG_SEGMENTS
            || self.cancellation.grace_ms == 0
        {
            return Err(ParallelProofError::InvalidField("canary job policy"));
        }
        self.operation.validate()?;
        self.operation.digest()?;
        Ok(())
    }

    /// Digest of every immutable execution input and predicate.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        self.validate()?;
        domain_digest("shipyard.canary-job.envelope.v1", self)
    }
}

/// OS/backend identity required to distinguish PID reuse and descendants.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryProcessTreeIdentity {
    /// Direct child process identifier.
    pub pid: u32,
    /// Process-group or job-object leader identifier.
    pub tree_id: String,
    /// Adapter-authenticated process birth token.
    pub birth_token: String,
    /// Nonce derived from the immutable prepared receipt and supplied at launch.
    pub launch_nonce_sha256: Sha256Digest,
    /// Exact executable digest observed at launch.
    pub executable_sha256: Sha256Digest,
    /// Controller launch timestamp.
    pub launched_at_ms: u64,
}

impl CanaryProcessTreeIdentity {
    fn validate(
        &self,
        job: &ApprovedCanaryJob,
        expected_nonce: &Sha256Digest,
    ) -> Result<(), ParallelProofError> {
        validate_id(&self.tree_id, "canary process tree id")?;
        validate_id(&self.birth_token, "canary process birth token")?;
        let ApprovedCanaryOperation::ParallelProofDistributedShadow {
            adapter_executable_sha256,
            ..
        } = &job.operation;
        if self.pid == 0
            || self.launched_at_ms < job.approved_at_ms
            || self.launched_at_ms >= job.deadline_at_ms
            || self.launch_nonce_sha256 != *expected_nonce
            || self.executable_sha256 != *adapter_executable_sha256
        {
            return Err(ParallelProofError::BindingMismatch(
                "canary process identity",
            ));
        }
        Ok(())
    }
}

/// Authenticated response artifact from the typed provider adapter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryJobArtifact {
    /// Typed response schema.
    pub schema_version: u32,
    /// Digest of the exact approved operation.
    pub operation_sha256: Sha256Digest,
    /// Digest of the complete response bytes.
    pub content_sha256: Sha256Digest,
    /// Response byte length.
    pub bytes: u32,
}

/// Decoded typed response persisted before a success receipt can be considered.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryJobResponse {
    /// Exact response schema selected by the immutable success predicate.
    pub schema_version: u32,
    /// Exact approved operation digest.
    pub operation_sha256: Sha256Digest,
    /// Exact immutable job envelope digest.
    pub job_sha256: Sha256Digest,
    /// Exact immutable launch nonce for this job attempt.
    pub launch_nonce_sha256: Sha256Digest,
    /// Existing typed distributed-canary observation.
    pub observation: DistributedExecutionObservation,
}

impl CanaryJobResponse {
    fn validate(
        &self,
        job: &ApprovedCanaryJob,
        expected_launch_nonce_sha256: &Sha256Digest,
    ) -> Result<(), ParallelProofError> {
        if self.schema_version != job.success.artifact_schema_version
            || self.operation_sha256 != job.operation.digest()?
            || self.job_sha256 != job.digest()?
            || self.launch_nonce_sha256 != *expected_launch_nonce_sha256
        {
            return Err(ParallelProofError::BindingMismatch(
                "canary response identity",
            ));
        }
        let ApprovedCanaryOperation::ParallelProofDistributedShadow {
            artifact_bytes_total,
            ..
        } = &job.operation;
        validate_distributed_observation(*artifact_bytes_total, &self.observation)
    }
}

/// One backend observation of the originally launched process tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanaryProcessObservation {
    /// Exact process tree remains alive.
    Alive(CanaryProcessTreeIdentity),
    /// Exact process tree exited and optional response metadata was recovered.
    Exited {
        /// Original process identity.
        process: CanaryProcessTreeIdentity,
        /// OS exit code, absent only when the backend cannot recover it.
        exit_code: Option<i32>,
        /// Adapter-authenticated controller time when exit was observed.
        exited_at_ms: u64,
        /// Typed response artifact, if durably complete.
        artifact: Option<CanaryJobArtifact>,
    },
    /// No process matching the launch nonce exists.
    Missing,
    /// A PID exists but its birth token, group, nonce, or executable differs.
    IdentityMismatch,
}

/// Result of a bounded complete-tree cancellation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanaryCancellationObservation {
    /// Backend proved the complete admitted process tree terminated.
    Terminated,
    /// The process tree remained alive after the complete grace bound.
    StillAlive,
    /// The backend could no longer identify the admitted process tree.
    Missing,
}

/// Provider-neutral process mechanics for one closed typed operation.
pub trait CanaryJobBackend {
    /// Launch the exact approved operation once, bound to `launch_nonce_sha256`.
    fn launch(
        &mut self,
        job: &ApprovedCanaryJob,
        launch_nonce_sha256: &Sha256Digest,
        claimed_at_ms: u64,
    ) -> Result<CanaryProcessTreeIdentity, String>;

    /// Discover an original launch after a crash between spawn and running receipt.
    fn discover(
        &mut self,
        job: &ApprovedCanaryJob,
        launch_nonce_sha256: &Sha256Digest,
    ) -> Result<CanaryProcessObservation, String>;

    /// Observe the exact process identity without launching anything.
    fn observe(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
    ) -> Result<CanaryProcessObservation, String>;

    /// Request and prove bounded complete-tree cancellation.
    fn cancel(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
        grace_ms: u64,
    ) -> Result<CanaryCancellationObservation, String>;
}

/// Authenticated immutable cancellation request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryCancellationRequest {
    /// Exact job envelope digest.
    pub job_sha256: Sha256Digest,
    /// Requesting controller identity.
    pub controller_id: String,
    /// Approval authority reused for cancellation.
    pub approval_sha256: Sha256Digest,
    /// Controller request timestamp.
    pub requested_at_ms: u64,
}

/// Authenticated durable acknowledgement of one terminal wake receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryWakeAcknowledgement {
    /// Exact immutable job digest.
    pub job_sha256: Sha256Digest,
    /// Exact terminal receipt digest used as the idempotency key.
    pub receipt_sha256: Sha256Digest,
    /// Acknowledging controller identity.
    pub controller_id: String,
    /// Approval authority reused for wake acknowledgement.
    pub approval_sha256: Sha256Digest,
    /// Controller acknowledgement timestamp.
    pub acknowledged_at_ms: u64,
}

/// Terminal disposition. Only `Succeeded` represents successful work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryJobTerminalOutcome {
    /// Exit and typed artifact matched every immutable predicate.
    Succeeded,
    /// The original process exited without satisfying success predicates.
    Failed,
    /// Complete-tree termination was proven after an authenticated request/deadline.
    Cancelled,
    /// Authenticated cancellation won immutable arbitration before any launch.
    CancelledBeforeLaunch,
    /// Complete-tree termination could not be proven within its bound.
    CancellationUncertain,
    /// The original launch or process identity disappeared without a terminal artifact.
    Lost,
    /// Immutable heartbeat capacity was exhausted before a terminal observation.
    HeartbeatLimit,
}

/// Immutable receipt payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CanaryJobReceiptState {
    /// Intent persisted before launch.
    Prepared {
        /// Nonce that the backend must bind into process identity.
        launch_nonce_sha256: Sha256Digest,
    },
    /// Exclusive immutable authority to call the backend launch operation.
    Launching {
        /// Exact launch nonce that backend discovery must use after a crash.
        launch_nonce_sha256: Sha256Digest,
        /// Controller time when exclusive launch authority was acquired.
        claimed_at_ms: u64,
    },
    /// Original process identity persisted after launch/discovery.
    Running {
        /// Exact process tree identity.
        process: CanaryProcessTreeIdentity,
    },
    /// Immutable liveness observation for the same process identity.
    Heartbeat {
        /// Exact process tree identity.
        process: CanaryProcessTreeIdentity,
        /// Controller observation time.
        observed_at_ms: u64,
    },
    /// Authenticated cancellation won sequence arbitration before terminal state.
    CancellationRequested {
        /// Exact process tree still under custody.
        process: CanaryProcessTreeIdentity,
        /// Digest of the complete authenticated cancellation request.
        request_sha256: Sha256Digest,
        /// Controller request time.
        requested_at_ms: u64,
    },
    /// Cancellation requested after launch claim but before identity discovery.
    CancellationRequestedBeforeIdentity {
        /// Exact immutable launch nonce under custody.
        launch_nonce_sha256: Sha256Digest,
        /// Digest of the complete authenticated cancellation request.
        request_sha256: Sha256Digest,
        /// Controller request time.
        requested_at_ms: u64,
    },
    /// Terminal receipt; no later receipt is legal.
    Terminal {
        /// Exact terminal classification.
        outcome: CanaryJobTerminalOutcome,
        /// Last known process identity, if one was authenticated.
        process: Option<CanaryProcessTreeIdentity>,
        /// Validated response artifact for success; absent otherwise.
        artifact: Option<CanaryJobArtifact>,
        /// Domain-separated digest of bounded backend error text, if any.
        failure_sha256: Option<Sha256Digest>,
        /// Controller terminal time.
        completed_at_ms: u64,
    },
}

/// Hash-chained immutable lifecycle receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryJobReceipt {
    /// Receipt schema.
    pub schema_version: u32,
    /// Exact immutable job digest.
    pub job_sha256: Sha256Digest,
    /// Contiguous receipt sequence.
    pub sequence: u32,
    /// Digest of the preceding receipt, absent only for sequence zero.
    pub previous_receipt_sha256: Option<Sha256Digest>,
    /// Lifecycle payload.
    pub receipt: CanaryJobReceiptState,
}

impl CanaryJobReceipt {
    /// Digest the complete receipt after structural validation.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        if self.schema_version != SCHEMA_VERSION
            || (self.sequence == 0) != self.previous_receipt_sha256.is_none()
        {
            return Err(ParallelProofError::CorruptRecord(
                "canary job receipt chain".to_owned(),
            ));
        }
        domain_digest("shipyard.canary-job.receipt.v1", self)
    }
}

/// Reconstructed durable job state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanaryJobSnapshot {
    /// Exact immutable job.
    pub job: ApprovedCanaryJob,
    /// Complete contiguous receipt chain.
    pub receipts: Vec<CanaryJobReceipt>,
}

impl CanaryJobSnapshot {
    /// Latest lifecycle receipt.
    #[must_use]
    pub fn latest(&self) -> &CanaryJobReceipt {
        self.receipts
            .last()
            .expect("validated snapshot always has a prepared receipt")
    }

    /// Whether a terminal receipt exists.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.latest().receipt,
            CanaryJobReceiptState::Terminal { .. }
        )
    }
}

/// Result of launch or restart reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanaryJobTransition {
    /// Reconstructed state after the transition.
    pub snapshot: CanaryJobSnapshot,
    /// Whether the immutable wake predicate selects the terminal receipt.
    pub wake: bool,
    /// Terminal receipt sequence selected for idempotent wake delivery.
    pub wake_receipt_sequence: Option<u32>,
    /// Digest-only retryable backend failure; custody remains nonterminal.
    pub retryable_failure_sha256: Option<Sha256Digest>,
    /// Whether the operation was launched during this call.
    pub launched: bool,
}

/// Crash-consistent immutable custody store for approved canary jobs.
#[derive(Clone, Debug)]
pub struct CanaryJobStore {
    records: ImmutableByteStore,
    artifacts: ImmutableByteStore,
    logs: ImmutableByteStore,
}

impl CanaryJobStore {
    /// Open a private controller-owned job root.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ParallelProofError> {
        let root = root.into();
        crate::writer_domain_lease::ensure_protected_dir_all(&root)
            .map_err(ParallelProofError::Io)?;
        Ok(Self {
            records: ImmutableByteStore::open(root.join("records"), MAX_RECORD_BYTES)
                .map_err(map_store_error)?,
            artifacts: ImmutableByteStore::open(root.join("artifacts"), MAX_RECORD_BYTES)
                .map_err(map_store_error)?,
            logs: ImmutableByteStore::open(root.join("logs"), MAX_RECORD_BYTES)
                .map_err(map_store_error)?,
        })
    }

    /// Persist immutable intent before any backend launch is legal.
    pub fn submit(&self, job: &ApprovedCanaryJob) -> Result<StoreWriteOutcome, ParallelProofError> {
        job.validate()?;
        let job_sha256 = job.digest()?;
        let envelope_outcome = self
            .records
            .put(&envelope_key(&job.job_id), &serde_json::to_vec(job)?)
            .map_err(map_store_error)?;
        let launch_nonce_sha256 = domain_digest(
            "shipyard.canary-job.launch-nonce.v1",
            &(job_sha256.clone(), &job.owner.controller_incarnation),
        )?;
        let prepared = CanaryJobReceipt {
            schema_version: SCHEMA_VERSION,
            job_sha256,
            sequence: 0,
            previous_receipt_sha256: None,
            receipt: CanaryJobReceiptState::Prepared {
                launch_nonce_sha256,
            },
        };
        validate_receipt(job, &[], &prepared, &prepared.job_sha256)?;
        let receipt_outcome = self.put_receipt(&job.job_id, &prepared)?;
        Ok(
            if envelope_outcome == StoreWriteOutcome::Created
                || receipt_outcome == StoreWriteOutcome::Created
            {
                StoreWriteOutcome::Created
            } else {
                StoreWriteOutcome::AlreadyPresent
            },
        )
    }

    /// Load and validate the complete bounded immutable receipt chain.
    pub fn load(&self, job_id: &str) -> Result<CanaryJobSnapshot, ParallelProofError> {
        let snapshot = self.load_receipt_chain(job_id)?;
        if let CanaryJobReceiptState::Terminal {
            outcome: CanaryJobTerminalOutcome::Succeeded,
            artifact: Some(artifact),
            ..
        } = &snapshot.latest().receipt
            && !self.artifact_matches(job_id, artifact)?
        {
            return Err(ParallelProofError::CorruptRecord(
                "canary job success artifact".to_owned(),
            ));
        }
        Ok(snapshot)
    }

    fn load_receipt_chain(&self, job_id: &str) -> Result<CanaryJobSnapshot, ParallelProofError> {
        validate_id(job_id, "canary job id")?;
        let job: ApprovedCanaryJob = serde_json::from_slice(
            &self
                .records
                .load(&envelope_key(job_id))
                .map_err(map_store_error)?,
        )?;
        job.validate()?;
        if job.job_id != job_id {
            return Err(ParallelProofError::CorruptRecord(
                "canary job logical key".to_owned(),
            ));
        }
        let job_sha256 = job.digest()?;
        let mut receipts = Vec::new();
        let maximum = job.max_heartbeat_receipts + 5;
        for sequence in 0..maximum {
            let bytes = match self.records.load(&receipt_key(job_id, sequence)) {
                Ok(bytes) => bytes,
                Err(ImmutableStoreError::Missing(_)) => break,
                Err(error) => return Err(map_store_error(error)),
            };
            let receipt: CanaryJobReceipt = serde_json::from_slice(&bytes)?;
            validate_receipt(&job, &receipts, &receipt, &job_sha256)?;
            receipts.push(receipt);
        }
        if receipts.is_empty() {
            return Err(ParallelProofError::CorruptRecord(
                "canary job missing prepared receipt".to_owned(),
            ));
        }
        if self
            .records
            .contains(&receipt_key(job_id, maximum))
            .map_err(map_store_error)?
        {
            return Err(ParallelProofError::CorruptRecord(
                "canary job receipt limit".to_owned(),
            ));
        }
        Ok(CanaryJobSnapshot { job, receipts })
    }

    /// Persist an authenticated cancel request without overwriting a contradiction.
    pub fn request_cancel(
        &self,
        job_id: &str,
        request: &CanaryCancellationRequest,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        for _ in 0..3 {
            let result = self.request_cancel_once(job_id, request);
            if !matches!(result, Err(ParallelProofError::ImmutableConflict(_))) {
                return result;
            }
        }
        Err(ParallelProofError::ImmutableConflict(job_id.to_owned()))
    }

    // Keep the same-sequence cancellation arbitration contiguous and auditable.
    #[allow(clippy::too_many_lines)]
    fn request_cancel_once(
        &self,
        job_id: &str,
        request: &CanaryCancellationRequest,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        let snapshot = self.load(job_id)?;
        validate_id(&request.controller_id, "canary cancel controller")?;
        if request.job_sha256 != snapshot.job.digest()?
            || request.controller_id != snapshot.job.owner.controller_id
            || request.approval_sha256 != snapshot.job.owner.approval_sha256
            || request.requested_at_ms < snapshot.job.approved_at_ms
        {
            return Err(ParallelProofError::AuthenticationFailed);
        }
        if let CanaryJobReceiptState::Terminal { outcome, .. } = snapshot.latest().receipt {
            return if outcome == CanaryJobTerminalOutcome::CancelledBeforeLaunch {
                Ok(StoreWriteOutcome::AlreadyPresent)
            } else {
                Err(ParallelProofError::InvalidAttemptSequence(
                    "canary cancellation lost terminal arbitration".to_owned(),
                ))
            };
        }
        if matches!(
            snapshot.latest().receipt,
            CanaryJobReceiptState::Prepared { .. }
        ) {
            let terminal = terminal_receipt(
                CanaryJobTerminalOutcome::CancelledBeforeLaunch,
                None,
                None,
                None,
                request.requested_at_ms,
            )?;
            let previous = snapshot.latest();
            let receipt = CanaryJobReceipt {
                schema_version: SCHEMA_VERSION,
                job_sha256: snapshot.job.digest()?,
                sequence: previous.sequence + 1,
                previous_receipt_sha256: Some(previous.digest()?),
                receipt: terminal,
            };
            validate_receipt(
                &snapshot.job,
                &snapshot.receipts,
                &receipt,
                &receipt.job_sha256,
            )?;
            match self.put_receipt(job_id, &receipt) {
                Ok(outcome) => return Ok(outcome),
                Err(ParallelProofError::ImmutableConflict(_)) => {
                    // Launch won the same-sequence arbitration. Continue by
                    // competing a cancellation receipt with its next state.
                }
                Err(error) => return Err(error),
            }
        }
        let snapshot = self.load(job_id)?;
        let request_sha256 = domain_digest("shipyard.canary-job.cancel-request.v1", request)?;
        if let CanaryJobReceiptState::CancellationRequested {
            request_sha256: existing,
            ..
        }
        | CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
            request_sha256: existing,
            ..
        } = &snapshot.latest().receipt
        {
            return if *existing == request_sha256 {
                Ok(StoreWriteOutcome::AlreadyPresent)
            } else {
                Err(ParallelProofError::ImmutableConflict(job_id.to_owned()))
            };
        }
        if snapshot.is_terminal() {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary cancellation lost terminal arbitration".to_owned(),
            ));
        }
        let (launch_nonce_sha256, process) = active_identity(&snapshot)?;
        let previous = snapshot.latest();
        let cancellation_state = if let Some(process) = process.cloned() {
            CanaryJobReceiptState::CancellationRequested {
                process,
                request_sha256: request_sha256.clone(),
                requested_at_ms: request.requested_at_ms,
            }
        } else if matches!(
            snapshot.latest().receipt,
            CanaryJobReceiptState::Launching { .. }
        ) {
            CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
                launch_nonce_sha256: launch_nonce_sha256.clone(),
                request_sha256: request_sha256.clone(),
                requested_at_ms: request.requested_at_ms,
            }
        } else {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary cancellation has no launch custody".to_owned(),
            ));
        };
        let receipt = CanaryJobReceipt {
            schema_version: SCHEMA_VERSION,
            job_sha256: snapshot.job.digest()?,
            sequence: previous.sequence + 1,
            previous_receipt_sha256: Some(previous.digest()?),
            receipt: cancellation_state,
        };
        validate_receipt(
            &snapshot.job,
            &snapshot.receipts,
            &receipt,
            &receipt.job_sha256,
        )?;
        match self.put_receipt(job_id, &receipt) {
            Ok(outcome) => Ok(outcome),
            Err(ParallelProofError::ImmutableConflict(_)) => {
                let latest = self.load(job_id)?;
                match &latest.latest().receipt {
                    CanaryJobReceiptState::CancellationRequested {
                        request_sha256: existing,
                        ..
                    }
                    | CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
                        request_sha256: existing,
                        ..
                    } if *existing == request_sha256 => Ok(StoreWriteOutcome::AlreadyPresent),
                    CanaryJobReceiptState::Terminal { .. } => {
                        Err(ParallelProofError::InvalidAttemptSequence(
                            "canary cancellation lost terminal arbitration".to_owned(),
                        ))
                    }
                    _ => Err(ParallelProofError::ImmutableConflict(job_id.to_owned())),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Durably acknowledge delivery of the selected terminal wake.
    pub fn acknowledge_wake(
        &self,
        job_id: &str,
        acknowledgement: &CanaryWakeAcknowledgement,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        let snapshot = self.load(job_id)?;
        let CanaryJobReceiptState::Terminal {
            outcome,
            completed_at_ms,
            ..
        } = &snapshot.latest().receipt
        else {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary wake is not terminal".to_owned(),
            ));
        };
        if !selected_for_wake(&snapshot.job, *outcome)
            || acknowledgement.job_sha256 != snapshot.job.digest()?
            || acknowledgement.receipt_sha256 != snapshot.latest().digest()?
            || acknowledgement.controller_id != snapshot.job.owner.controller_id
            || acknowledgement.approval_sha256 != snapshot.job.owner.approval_sha256
            || acknowledgement.acknowledged_at_ms < *completed_at_ms
        {
            return Err(ParallelProofError::AuthenticationFailed);
        }
        self.records
            .put(&wake_ack_key(job_id), &serde_json::to_vec(acknowledgement)?)
            .map_err(map_store_error)
    }

    /// Record one redacted immutable log segment. Segments rotate by sequence and cap.
    pub fn record_log_segment(
        &self,
        job_id: &str,
        sequence: u32,
        bytes: &[u8],
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        let snapshot = self.load(job_id)?;
        if sequence >= snapshot.job.logs.max_segments {
            return Err(ParallelProofError::LimitExceeded {
                field: "canary log segments",
                max: snapshot.job.logs.max_segments as usize,
                found: sequence as usize + 1,
            });
        }
        let redacted = redact_log(bytes, snapshot.job.logs.segment_bytes as usize)?;
        self.logs
            .put(&log_key(job_id, sequence), &redacted)
            .map_err(map_store_error)
    }

    /// Persist the exact typed response bytes before a success receipt is legal.
    pub fn record_artifact(
        &self,
        job_id: &str,
        response: &CanaryJobResponse,
    ) -> Result<CanaryJobArtifact, ParallelProofError> {
        let snapshot = self.load(job_id)?;
        if matches!(
            snapshot.latest().receipt,
            CanaryJobReceiptState::Prepared { .. }
                | CanaryJobReceiptState::CancellationRequestedBeforeIdentity { .. }
                | CanaryJobReceiptState::Terminal { .. }
        ) {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary artifact requires launch custody".to_owned(),
            ));
        }
        let (launch_nonce_sha256, _) = active_identity(&snapshot)?;
        response.validate(&snapshot.job, launch_nonce_sha256)?;
        let bytes = serde_json::to_vec(response)?;
        if bytes.is_empty() || bytes.len() > snapshot.job.success.max_artifact_bytes as usize {
            return Err(ParallelProofError::LimitExceeded {
                field: "canary job artifact bytes",
                max: snapshot.job.success.max_artifact_bytes as usize,
                found: bytes.len(),
            });
        }
        let artifact = CanaryJobArtifact {
            schema_version: snapshot.job.success.artifact_schema_version,
            operation_sha256: snapshot.job.operation.digest()?,
            content_sha256: Sha256Digest::of_bytes(&bytes),
            bytes: u32::try_from(bytes.len())
                .map_err(|_| ParallelProofError::InvalidField("canary artifact size"))?,
        };
        self.artifacts
            .put(&artifact_key(job_id), &bytes)
            .map_err(map_store_error)?;
        Ok(artifact)
    }

    fn artifact_matches(
        &self,
        job_id: &str,
        artifact: &CanaryJobArtifact,
    ) -> Result<bool, ParallelProofError> {
        match self.artifacts.load(&artifact_key(job_id)) {
            Ok(bytes) => {
                let snapshot = self.load_receipt_chain(job_id)?;
                let response: CanaryJobResponse = serde_json::from_slice(&bytes)?;
                let (launch_nonce_sha256, _) = active_identity(&snapshot)?;
                response.validate(&snapshot.job, launch_nonce_sha256)?;
                Ok(bytes.len() == artifact.bytes as usize
                    && Sha256Digest::of_bytes(&bytes) == artifact.content_sha256)
            }
            Err(ImmutableStoreError::Missing(_)) => Ok(false),
            Err(error) => Err(map_store_error(error)),
        }
    }

    /// Load one already-redacted immutable log segment.
    pub fn load_log_segment(
        &self,
        job_id: &str,
        sequence: u32,
    ) -> Result<Vec<u8>, ParallelProofError> {
        let snapshot = self.load(job_id)?;
        if sequence >= snapshot.job.logs.max_segments {
            return Err(ParallelProofError::InvalidField("canary log sequence"));
        }
        self.logs
            .load(&log_key(job_id, sequence))
            .map_err(map_store_error)
    }

    fn cancellation_requested(snapshot: &CanaryJobSnapshot) -> bool {
        matches!(
            snapshot.latest().receipt,
            CanaryJobReceiptState::CancellationRequested { .. }
                | CanaryJobReceiptState::CancellationRequestedBeforeIdentity { .. }
        )
    }

    fn cancellation_requested_at_ms(snapshot: &CanaryJobSnapshot) -> Option<u64> {
        match snapshot.latest().receipt {
            CanaryJobReceiptState::CancellationRequested {
                requested_at_ms, ..
            }
            | CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
                requested_at_ms, ..
            } => Some(requested_at_ms),
            _ => None,
        }
    }

    fn wake_pending(&self, snapshot: &CanaryJobSnapshot) -> Result<bool, ParallelProofError> {
        let CanaryJobReceiptState::Terminal {
            outcome,
            completed_at_ms,
            ..
        } = &snapshot.latest().receipt
        else {
            return Ok(false);
        };
        if !selected_for_wake(&snapshot.job, *outcome) {
            return Ok(false);
        }
        match self.records.load(&wake_ack_key(&snapshot.job.job_id)) {
            Ok(bytes) => {
                let acknowledgement: CanaryWakeAcknowledgement = serde_json::from_slice(&bytes)?;
                if acknowledgement.job_sha256 != snapshot.job.digest()?
                    || acknowledgement.receipt_sha256 != snapshot.latest().digest()?
                    || acknowledgement.controller_id != snapshot.job.owner.controller_id
                    || acknowledgement.approval_sha256 != snapshot.job.owner.approval_sha256
                    || acknowledgement.acknowledged_at_ms < *completed_at_ms
                {
                    return Err(ParallelProofError::AuthenticationFailed);
                }
                Ok(false)
            }
            Err(ImmutableStoreError::Missing(_)) => Ok(true),
            Err(error) => Err(map_store_error(error)),
        }
    }

    fn append(
        &self,
        snapshot: &CanaryJobSnapshot,
        receipt: CanaryJobReceiptState,
    ) -> Result<CanaryJobSnapshot, ParallelProofError> {
        if snapshot.is_terminal() {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary job is terminal".to_owned(),
            ));
        }
        let previous = snapshot.latest();
        let next = CanaryJobReceipt {
            schema_version: SCHEMA_VERSION,
            job_sha256: snapshot.job.digest()?,
            sequence: previous.sequence + 1,
            previous_receipt_sha256: Some(previous.digest()?),
            receipt,
        };
        validate_receipt(&snapshot.job, &snapshot.receipts, &next, &next.job_sha256)?;
        self.put_receipt(&snapshot.job.job_id, &next)?;
        self.load(&snapshot.job.job_id)
    }

    fn claim_launch(
        &self,
        snapshot: &CanaryJobSnapshot,
        launch_nonce_sha256: Sha256Digest,
        claimed_at_ms: u64,
    ) -> Result<(CanaryJobSnapshot, StoreWriteOutcome), ParallelProofError> {
        let previous = snapshot.latest();
        let claim = CanaryJobReceipt {
            schema_version: SCHEMA_VERSION,
            job_sha256: snapshot.job.digest()?,
            sequence: previous.sequence + 1,
            previous_receipt_sha256: Some(previous.digest()?),
            receipt: CanaryJobReceiptState::Launching {
                launch_nonce_sha256,
                claimed_at_ms,
            },
        };
        validate_receipt(&snapshot.job, &snapshot.receipts, &claim, &claim.job_sha256)?;
        let outcome = self.put_receipt(&snapshot.job.job_id, &claim)?;
        Ok((self.load(&snapshot.job.job_id)?, outcome))
    }

    fn put_receipt(
        &self,
        job_id: &str,
        receipt: &CanaryJobReceipt,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        receipt.digest()?;
        self.records
            .put(
                &receipt_key(job_id, receipt.sequence),
                &serde_json::to_vec(receipt)?,
            )
            .map_err(map_store_error)
    }
}

/// Persist intent and launch exactly once. A replay only returns durable state.
pub fn launch_canary_job<B: CanaryJobBackend>(
    store: &CanaryJobStore,
    job: &ApprovedCanaryJob,
    controller_now_ms: u64,
    backend: &mut B,
) -> Result<CanaryJobTransition, ParallelProofError> {
    if controller_now_ms < job.approved_at_ms || controller_now_ms >= job.deadline_at_ms {
        return Err(ParallelProofError::InvalidField(
            "canary launch controller time",
        ));
    }
    store.submit(job)?;
    let snapshot = store.load(&job.job_id)?;
    if snapshot.receipts.len() != 1 {
        return replay_transition(store, snapshot);
    }
    let CanaryJobReceiptState::Prepared {
        launch_nonce_sha256,
    } = &snapshot.latest().receipt
    else {
        return Err(ParallelProofError::CorruptRecord(
            "canary initial receipt".to_owned(),
        ));
    };
    let launch_nonce_sha256 = launch_nonce_sha256.clone();
    let (snapshot, claim_outcome) =
        match store.claim_launch(&snapshot, launch_nonce_sha256.clone(), controller_now_ms) {
            Ok(claim) => claim,
            Err(ParallelProofError::ImmutableConflict(_)) => {
                return replay_transition(store, store.load(&job.job_id)?);
            }
            Err(error) => return Err(error),
        };
    if claim_outcome != StoreWriteOutcome::Created {
        return Ok(transition(snapshot, false));
    }
    let process = match backend.launch(job, &launch_nonce_sha256, controller_now_ms) {
        Ok(process) => process,
        Err(error) => {
            let failure = domain_digest("shipyard.canary-job.launch-error.v1", &error)?;
            return Ok(retryable_transition(snapshot, failure));
        }
    };
    process.validate(job, &launch_nonce_sha256)?;
    let snapshot = store.append(&snapshot, CanaryJobReceiptState::Running { process })?;
    Ok(CanaryJobTransition {
        snapshot,
        wake: false,
        wake_receipt_sequence: None,
        retryable_failure_sha256: None,
        launched: true,
    })
}

/// Reconcile one existing job without ever redispatching it.
pub fn reconcile_canary_job<B: CanaryJobBackend>(
    store: &CanaryJobStore,
    job_id: &str,
    controller_now_ms: u64,
    backend: &mut B,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let snapshot = store.load(job_id)?;
    if snapshot.is_terminal() {
        return replay_transition(store, snapshot);
    }
    if matches!(
        snapshot.latest().receipt,
        CanaryJobReceiptState::Prepared { .. }
    ) {
        // Submission alone is not proof that launch was attempted. Reconciliation
        // never dispatches; a later authorized launch invocation may acquire the
        // immutable launch claim.
        return Ok(transition(snapshot, false));
    }
    if controller_now_ms < last_observed_at_ms(&snapshot) {
        return Err(ParallelProofError::InvalidField(
            "canary reconcile controller time",
        ));
    }
    let (launch_nonce, process) = active_identity(&snapshot)?;
    let observation = if let Some(process) = process {
        backend.observe(&snapshot.job, process)
    } else {
        backend.discover(&snapshot.job, launch_nonce)
    };
    let observation = match observation {
        Ok(observation) => observation,
        Err(error) => {
            let failure = domain_digest("shipyard.canary-job.observation-error.v1", &error)?;
            return Ok(retryable_transition(snapshot, failure));
        }
    };
    reconcile_observation(
        store,
        &snapshot,
        controller_now_ms,
        launch_nonce,
        process,
        observation,
        backend,
    )
}

fn reconcile_observation<B: CanaryJobBackend>(
    store: &CanaryJobStore,
    snapshot: &CanaryJobSnapshot,
    controller_now_ms: u64,
    launch_nonce: &Sha256Digest,
    process: Option<&CanaryProcessTreeIdentity>,
    observation: CanaryProcessObservation,
    backend: &mut B,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let cancel_requested = CanaryJobStore::cancellation_requested(snapshot);
    let deadline_cancel = snapshot.job.cancellation.cancel_at_deadline
        && controller_now_ms >= snapshot.job.deadline_at_ms;
    let heartbeat_expired = process.is_some()
        && controller_now_ms.saturating_sub(last_observed_at_ms(snapshot))
            > snapshot.job.heartbeat_timeout_ms;
    match observation {
        CanaryProcessObservation::Alive(observed) => {
            observed.validate(&snapshot.job, launch_nonce)?;
            if let Some(expected) = process
                && observed != *expected
            {
                return finish_lost(store, snapshot, controller_now_ms, None);
            }
            if cancel_requested || deadline_cancel || heartbeat_expired {
                return reconcile_cancellation(
                    store,
                    snapshot,
                    &observed,
                    controller_now_ms,
                    CanaryJobTerminalOutcome::Cancelled,
                    backend,
                );
            }
            if process.is_some()
                && controller_now_ms.saturating_sub(last_observed_at_ms(snapshot))
                    < snapshot.job.heartbeat_interval_ms
            {
                return Ok(transition(snapshot.clone(), false));
            }
            let heartbeat_count = heartbeat_count(snapshot)?;
            if heartbeat_count >= snapshot.job.max_heartbeat_receipts {
                return reconcile_cancellation(
                    store,
                    snapshot,
                    &observed,
                    controller_now_ms,
                    CanaryJobTerminalOutcome::HeartbeatLimit,
                    backend,
                );
            }
            let receipt = if process.is_none() {
                CanaryJobReceiptState::Running { process: observed }
            } else {
                CanaryJobReceiptState::Heartbeat {
                    process: observed,
                    observed_at_ms: controller_now_ms,
                }
            };
            Ok(transition(store.append(snapshot, receipt)?, false))
        }
        CanaryProcessObservation::Exited {
            process: observed,
            exit_code,
            exited_at_ms,
            artifact,
        } => {
            observed.validate(&snapshot.job, launch_nonce)?;
            if let Some(expected) = process
                && observed != *expected
            {
                return finish_lost(store, snapshot, controller_now_ms, None);
            }
            if CanaryJobStore::cancellation_requested_at_ms(snapshot)
                .is_some_and(|requested_at_ms| exited_at_ms >= requested_at_ms)
            {
                let terminal = terminal_receipt(
                    CanaryJobTerminalOutcome::Cancelled,
                    Some(observed),
                    None,
                    None,
                    controller_now_ms,
                )?;
                return Ok(terminal_transition(store.append(snapshot, terminal)?));
            }
            finish_exit(
                store,
                snapshot,
                observed,
                exit_code,
                exited_at_ms,
                artifact,
                controller_now_ms,
            )
        }
        CanaryProcessObservation::Missing | CanaryProcessObservation::IdentityMismatch => {
            finish_lost(store, snapshot, controller_now_ms, None)
        }
    }
}

fn last_observed_at_ms(snapshot: &CanaryJobSnapshot) -> u64 {
    snapshot
        .receipts
        .iter()
        .rev()
        .find_map(|receipt| match receipt.receipt {
            CanaryJobReceiptState::Heartbeat { observed_at_ms, .. } => Some(observed_at_ms),
            CanaryJobReceiptState::Running { ref process } => Some(process.launched_at_ms),
            CanaryJobReceiptState::Launching { claimed_at_ms, .. } => Some(claimed_at_ms),
            CanaryJobReceiptState::Prepared { .. } | CanaryJobReceiptState::Terminal { .. } => None,
            CanaryJobReceiptState::CancellationRequested {
                requested_at_ms, ..
            }
            | CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
                requested_at_ms, ..
            } => Some(requested_at_ms),
        })
        .unwrap_or(snapshot.job.approved_at_ms)
}

fn heartbeat_count(snapshot: &CanaryJobSnapshot) -> Result<u32, ParallelProofError> {
    u32::try_from(
        snapshot
            .receipts
            .iter()
            .filter(|receipt| matches!(receipt.receipt, CanaryJobReceiptState::Heartbeat { .. }))
            .count(),
    )
    .map_err(|_| ParallelProofError::CorruptRecord("canary heartbeat count".to_owned()))
}

fn reconcile_cancellation<B: CanaryJobBackend>(
    store: &CanaryJobStore,
    snapshot: &CanaryJobSnapshot,
    process: &CanaryProcessTreeIdentity,
    controller_now_ms: u64,
    terminated_outcome: CanaryJobTerminalOutcome,
    backend: &mut B,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let (outcome, failure) =
        match backend.cancel(&snapshot.job, process, snapshot.job.cancellation.grace_ms) {
            Ok(CanaryCancellationObservation::Terminated) => (terminated_outcome, None),
            Ok(CanaryCancellationObservation::StillAlive) => (
                CanaryJobTerminalOutcome::CancellationUncertain,
                Some("process tree remained alive"),
            ),
            Ok(CanaryCancellationObservation::Missing) => (
                CanaryJobTerminalOutcome::CancellationUncertain,
                Some("process identity disappeared during cancellation"),
            ),
            Err(error) => {
                let terminal = terminal_receipt(
                    CanaryJobTerminalOutcome::CancellationUncertain,
                    Some(process.clone()),
                    None,
                    Some(&error),
                    controller_now_ms,
                )?;
                return Ok(terminal_transition(store.append(snapshot, terminal)?));
            }
        };
    let terminal = terminal_receipt(
        outcome,
        Some(process.clone()),
        None,
        failure,
        controller_now_ms,
    )?;
    Ok(terminal_transition(store.append(snapshot, terminal)?))
}

fn finish_exit(
    store: &CanaryJobStore,
    snapshot: &CanaryJobSnapshot,
    process: CanaryProcessTreeIdentity,
    exit_code: Option<i32>,
    exited_at_ms: u64,
    artifact: Option<CanaryJobArtifact>,
    controller_now_ms: u64,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let operation_sha256 = snapshot.job.operation.digest()?;
    let valid_artifact = if let Some(artifact) = artifact.as_ref() {
        artifact.schema_version == snapshot.job.success.artifact_schema_version
            && artifact.operation_sha256 == operation_sha256
            && artifact.bytes > 0
            && artifact.bytes <= snapshot.job.success.max_artifact_bytes
            && store.artifact_matches(&snapshot.job.job_id, artifact)?
    } else {
        false
    };
    if exited_at_ms < process.launched_at_ms || exited_at_ms > controller_now_ms {
        return Err(ParallelProofError::BindingMismatch(
            "canary process exit time",
        ));
    }
    let within_deadline = !snapshot.job.cancellation.cancel_at_deadline
        || exited_at_ms <= snapshot.job.deadline_at_ms;
    let succeeded = exit_code == Some(snapshot.job.success.required_exit_code)
        && valid_artifact
        && within_deadline;
    let terminal = terminal_receipt(
        if succeeded {
            CanaryJobTerminalOutcome::Succeeded
        } else {
            CanaryJobTerminalOutcome::Failed
        },
        Some(process),
        succeeded.then_some(artifact).flatten(),
        (!succeeded).then_some("exit or artifact predicate failed"),
        controller_now_ms,
    )?;
    Ok(terminal_transition(store.append(snapshot, terminal)?))
}

fn finish_lost(
    store: &CanaryJobStore,
    snapshot: &CanaryJobSnapshot,
    controller_now_ms: u64,
    failure: Option<&str>,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let process = active_identity(snapshot)?.1.cloned();
    let terminal = terminal_receipt(
        CanaryJobTerminalOutcome::Lost,
        process,
        None,
        failure,
        controller_now_ms,
    )?;
    Ok(terminal_transition(store.append(snapshot, terminal)?))
}

fn active_identity(
    snapshot: &CanaryJobSnapshot,
) -> Result<(&Sha256Digest, Option<&CanaryProcessTreeIdentity>), ParallelProofError> {
    let CanaryJobReceiptState::Prepared {
        launch_nonce_sha256,
    } = &snapshot.receipts[0].receipt
    else {
        return Err(ParallelProofError::CorruptRecord(
            "canary prepared receipt".to_owned(),
        ));
    };
    let process = snapshot
        .receipts
        .iter()
        .rev()
        .find_map(|receipt| match &receipt.receipt {
            CanaryJobReceiptState::Running { process }
            | CanaryJobReceiptState::Heartbeat { process, .. }
            | CanaryJobReceiptState::CancellationRequested { process, .. } => Some(process),
            _ => None,
        });
    Ok((launch_nonce_sha256, process))
}

fn terminal_receipt(
    outcome: CanaryJobTerminalOutcome,
    process: Option<CanaryProcessTreeIdentity>,
    artifact: Option<CanaryJobArtifact>,
    failure: Option<&str>,
    completed_at_ms: u64,
) -> Result<CanaryJobReceiptState, ParallelProofError> {
    if completed_at_ms == 0
        || (outcome == CanaryJobTerminalOutcome::Succeeded) != artifact.is_some()
    {
        return Err(ParallelProofError::InvalidField("canary terminal receipt"));
    }
    Ok(CanaryJobReceiptState::Terminal {
        outcome,
        process,
        artifact,
        failure_sha256: failure
            .map(|message| domain_digest("shipyard.canary-job.failure.v1", &message))
            .transpose()?,
        completed_at_ms,
    })
}

fn transition(snapshot: CanaryJobSnapshot, launched: bool) -> CanaryJobTransition {
    CanaryJobTransition {
        snapshot,
        wake: false,
        wake_receipt_sequence: None,
        retryable_failure_sha256: None,
        launched,
    }
}

fn terminal_transition(snapshot: CanaryJobSnapshot) -> CanaryJobTransition {
    let wake = match snapshot.latest().receipt {
        CanaryJobReceiptState::Terminal { outcome, .. } => {
            selected_for_wake(&snapshot.job, outcome)
        }
        _ => false,
    };
    CanaryJobTransition {
        wake_receipt_sequence: wake.then_some(snapshot.latest().sequence),
        snapshot,
        wake,
        retryable_failure_sha256: None,
        launched: false,
    }
}

fn replay_transition(
    store: &CanaryJobStore,
    snapshot: CanaryJobSnapshot,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let wake = store.wake_pending(&snapshot)?;
    Ok(CanaryJobTransition {
        wake_receipt_sequence: wake.then_some(snapshot.latest().sequence),
        snapshot,
        wake,
        retryable_failure_sha256: None,
        launched: false,
    })
}

fn selected_for_wake(job: &ApprovedCanaryJob, outcome: CanaryJobTerminalOutcome) -> bool {
    match outcome {
        CanaryJobTerminalOutcome::Succeeded => job.wake.on_success,
        CanaryJobTerminalOutcome::Failed
        | CanaryJobTerminalOutcome::CancellationUncertain
        | CanaryJobTerminalOutcome::Lost
        | CanaryJobTerminalOutcome::HeartbeatLimit => job.wake.on_actionable_failure,
        CanaryJobTerminalOutcome::Cancelled | CanaryJobTerminalOutcome::CancelledBeforeLaunch => {
            false
        }
    }
}

fn retryable_transition(
    snapshot: CanaryJobSnapshot,
    retryable_failure_sha256: Sha256Digest,
) -> CanaryJobTransition {
    CanaryJobTransition {
        snapshot,
        wake: false,
        wake_receipt_sequence: None,
        retryable_failure_sha256: Some(retryable_failure_sha256),
        launched: false,
    }
}

// Receipt validation is intentionally one exhaustive state-transition table.
#[allow(clippy::too_many_lines)]
fn validate_receipt(
    job: &ApprovedCanaryJob,
    previous: &[CanaryJobReceipt],
    receipt: &CanaryJobReceipt,
    job_sha256: &Sha256Digest,
) -> Result<(), ParallelProofError> {
    receipt.digest()?;
    if receipt.job_sha256 != *job_sha256 || receipt.sequence as usize != previous.len() {
        return Err(ParallelProofError::CorruptRecord(
            "canary job receipt identity".to_owned(),
        ));
    }
    if let Some(prior) = previous.last()
        && (receipt.previous_receipt_sha256.as_ref() != Some(&prior.digest()?)
            || matches!(prior.receipt, CanaryJobReceiptState::Terminal { .. }))
    {
        return Err(ParallelProofError::CorruptRecord(
            "canary job receipt ordering".to_owned(),
        ));
    }
    let nonce = previous.first().and_then(|first| match &first.receipt {
        CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } => Some(launch_nonce_sha256),
        _ => None,
    });
    match &receipt.receipt {
        CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } if receipt.sequence == 0 => {
            let expected = domain_digest(
                "shipyard.canary-job.launch-nonce.v1",
                &(job_sha256, &job.owner.controller_incarnation),
            )?;
            if *launch_nonce_sha256 != expected {
                return Err(ParallelProofError::CorruptRecord(
                    "canary prepared launch nonce".to_owned(),
                ));
            }
        }
        CanaryJobReceiptState::Launching {
            launch_nonce_sha256,
            claimed_at_ms,
        } => {
            if !matches!(
                previous.last().map(|prior| &prior.receipt),
                Some(CanaryJobReceiptState::Prepared { .. })
            ) || Some(launch_nonce_sha256) != nonce
                || *claimed_at_ms < job.approved_at_ms
            {
                return Err(ParallelProofError::CorruptRecord(
                    "canary launching transition".to_owned(),
                ));
            }
        }
        CanaryJobReceiptState::Running { process } => {
            if !matches!(
                previous.last().map(|prior| &prior.receipt),
                Some(CanaryJobReceiptState::Launching { .. })
            ) {
                return Err(ParallelProofError::CorruptRecord(
                    "canary running transition".to_owned(),
                ));
            }
            process.validate(job, required_nonce(nonce)?)?;
            let claimed_at_ms = previous.last().and_then(|prior| match &prior.receipt {
                CanaryJobReceiptState::Launching { claimed_at_ms, .. } => Some(*claimed_at_ms),
                _ => None,
            });
            if claimed_at_ms != Some(process.launched_at_ms) {
                return Err(ParallelProofError::CorruptRecord(
                    "canary process launch claim time".to_owned(),
                ));
            }
        }
        CanaryJobReceiptState::Heartbeat {
            process,
            observed_at_ms,
        } => {
            let prior_process = previous.last().and_then(receipt_process).ok_or_else(|| {
                ParallelProofError::CorruptRecord("canary heartbeat transition".to_owned())
            })?;
            let prior_observed_at_ms = previous
                .last()
                .and_then(receipt_observed_at_ms)
                .unwrap_or(prior_process.launched_at_ms);
            if process != prior_process
                || *observed_at_ms < prior_observed_at_ms
                || *observed_at_ms < process.launched_at_ms
            {
                return Err(ParallelProofError::CorruptRecord(
                    "canary heartbeat identity or time".to_owned(),
                ));
            }
            process.validate(job, required_nonce(nonce)?)?;
        }
        CanaryJobReceiptState::CancellationRequested {
            process,
            requested_at_ms,
            ..
        } => {
            let prior_process = previous.last().and_then(receipt_process).ok_or_else(|| {
                ParallelProofError::CorruptRecord(
                    "canary cancellation request transition".to_owned(),
                )
            })?;
            let prior_observed_at_ms = previous
                .last()
                .and_then(receipt_observed_at_ms)
                .unwrap_or(prior_process.launched_at_ms);
            if process != prior_process || *requested_at_ms < prior_observed_at_ms {
                return Err(ParallelProofError::CorruptRecord(
                    "canary cancellation request identity or time".to_owned(),
                ));
            }
            process.validate(job, required_nonce(nonce)?)?;
        }
        CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
            launch_nonce_sha256,
            requested_at_ms,
            ..
        } => {
            let Some(CanaryJobReceiptState::Launching { claimed_at_ms, .. }) =
                previous.last().map(|prior| &prior.receipt)
            else {
                return Err(ParallelProofError::CorruptRecord(
                    "canary pre-identity cancellation transition".to_owned(),
                ));
            };
            if Some(launch_nonce_sha256) != nonce || *requested_at_ms < *claimed_at_ms {
                return Err(ParallelProofError::CorruptRecord(
                    "canary pre-identity cancellation binding".to_owned(),
                ));
            }
        }
        terminal @ CanaryJobReceiptState::Terminal { .. } => {
            validate_terminal_receipt(job, previous, terminal, nonce)?;
        }
        CanaryJobReceiptState::Prepared { .. } => {
            return Err(ParallelProofError::CorruptRecord(
                "canary job receipt transition".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_terminal_receipt(
    job: &ApprovedCanaryJob,
    previous: &[CanaryJobReceipt],
    terminal: &CanaryJobReceiptState,
    nonce: Option<&Sha256Digest>,
) -> Result<(), ParallelProofError> {
    let CanaryJobReceiptState::Terminal {
        outcome,
        process,
        artifact,
        completed_at_ms,
        ..
    } = terminal
    else {
        return Err(ParallelProofError::CorruptRecord(
            "canary terminal receipt type".to_owned(),
        ));
    };
    let prior_process = previous.iter().rev().find_map(receipt_process);
    let follows_launch_claim = matches!(
        previous.last().map(|prior| &prior.receipt),
        Some(
            CanaryJobReceiptState::Launching { .. }
                | CanaryJobReceiptState::CancellationRequestedBeforeIdentity { .. }
        )
    );
    let launch_claimed_at_ms = previous
        .iter()
        .rev()
        .find_map(|prior| match &prior.receipt {
            CanaryJobReceiptState::Launching { claimed_at_ms, .. } => Some(*claimed_at_ms),
            _ => None,
        });
    let prior_observed_at_ms = previous
        .iter()
        .rev()
        .find_map(receipt_observed_at_ms)
        .unwrap_or(job.approved_at_ms);
    if *completed_at_ms < prior_observed_at_ms
        || (*outcome == CanaryJobTerminalOutcome::Succeeded) != artifact.is_some()
        || (matches!(
            *outcome,
            CanaryJobTerminalOutcome::Succeeded
                | CanaryJobTerminalOutcome::Cancelled
                | CanaryJobTerminalOutcome::CancellationUncertain
                | CanaryJobTerminalOutcome::HeartbeatLimit
        ) && process.is_none())
        || prior_process.is_some_and(|prior| process.as_ref() != Some(prior))
        || (prior_process.is_none() && process.is_some() && !follows_launch_claim)
        || (follows_launch_claim
            && process
                .as_ref()
                .is_some_and(|process| Some(process.launched_at_ms) != launch_claimed_at_ms))
    {
        return Err(ParallelProofError::CorruptRecord(
            "canary terminal semantics".to_owned(),
        ));
    }
    if let Some(process) = process {
        process.validate(job, required_nonce(nonce)?)?;
    }
    if let Some(artifact) = artifact
        && (artifact.schema_version != job.success.artifact_schema_version
            || artifact.operation_sha256 != job.operation.digest()?
            || artifact.bytes == 0
            || artifact.bytes > job.success.max_artifact_bytes)
    {
        return Err(ParallelProofError::CorruptRecord(
            "canary terminal artifact predicate".to_owned(),
        ));
    }
    Ok(())
}

fn required_nonce(nonce: Option<&Sha256Digest>) -> Result<&Sha256Digest, ParallelProofError> {
    nonce.ok_or_else(|| ParallelProofError::CorruptRecord("canary launch nonce".to_owned()))
}

fn receipt_process(receipt: &CanaryJobReceipt) -> Option<&CanaryProcessTreeIdentity> {
    match &receipt.receipt {
        CanaryJobReceiptState::Running { process }
        | CanaryJobReceiptState::Heartbeat { process, .. }
        | CanaryJobReceiptState::CancellationRequested { process, .. } => Some(process),
        _ => None,
    }
}

fn receipt_observed_at_ms(receipt: &CanaryJobReceipt) -> Option<u64> {
    match &receipt.receipt {
        CanaryJobReceiptState::Launching { claimed_at_ms, .. } => Some(*claimed_at_ms),
        CanaryJobReceiptState::Running { process } => Some(process.launched_at_ms),
        CanaryJobReceiptState::Heartbeat { observed_at_ms, .. } => Some(*observed_at_ms),
        CanaryJobReceiptState::CancellationRequested {
            requested_at_ms, ..
        }
        | CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
            requested_at_ms, ..
        } => Some(*requested_at_ms),
        _ => None,
    }
}

fn validate_distributed_observation(
    expected_artifact_bytes: u64,
    observation: &DistributedExecutionObservation,
) -> Result<(), ParallelProofError> {
    let delivery = &observation.delivery;
    if delivery.artifact_bytes_total != expected_artifact_bytes
        || delivery
            .artifact_bytes_reused
            .checked_add(delivery.artifact_bytes_transferred)
            != Some(expected_artifact_bytes)
        || observation.submit_to_receipt_ms == 0
        || observation.worker_active_ms < observation.shard_execution_ms
    {
        return Err(ParallelProofError::BindingMismatch(
            "canary typed response counters",
        ));
    }
    match (delivery.mode, delivery.interruption.as_ref()) {
        (ArtifactDeliveryMode::FullTransfer, None)
            if delivery.artifact_bytes_reused == 0
                && delivery.artifact_bytes_transferred == expected_artifact_bytes =>
        {
            Ok(())
        }
        (ArtifactDeliveryMode::ImmutableObjectReuse, None)
            if delivery.artifact_bytes_reused == expected_artifact_bytes
                && delivery.artifact_bytes_transferred == 0 =>
        {
            Ok(())
        }
        (ArtifactDeliveryMode::VerifiedPrefixResume, Some(interruption))
            if interruption.verified_resume_offset_bytes == delivery.artifact_bytes_reused
                && interruption.bytes_before_interruption
                    >= interruption.verified_resume_offset_bytes
                && interruption.bytes_after_resume == delivery.artifact_bytes_transferred
                && interruption.verified_resume_offset_bytes > 0
                && interruption.verified_resume_offset_bytes < expected_artifact_bytes =>
        {
            Ok(())
        }
        _ => Err(ParallelProofError::InvalidField(
            "canary typed response delivery",
        )),
    }
}

fn redact_log(bytes: &[u8], max: usize) -> Result<Vec<u8>, ParallelProofError> {
    if bytes.len() > max {
        return Err(ParallelProofError::LimitExceeded {
            field: "canary log segment bytes",
            max,
            found: bytes.len(),
        });
    }
    let text = String::from_utf8_lossy(bytes);
    let mut output = String::with_capacity(text.len());
    for line in text.lines() {
        if safe_structured_log_line(line) {
            output.push_str(line);
        } else {
            output.push_str("[REDACTED]");
        }
        output.push('\n');
    }
    let output = output.into_bytes();
    if output.len() > max {
        return Err(ParallelProofError::LimitExceeded {
            field: "redacted canary log segment bytes",
            max,
            found: output.len(),
        });
    }
    Ok(output)
}

fn safe_structured_log_line(line: &str) -> bool {
    let Some((key, value)) = line.split_once('=') else {
        return false;
    };
    match key {
        "phase" => matches!(
            value,
            "prepare" | "transfer" | "verify" | "dispatch" | "aggregate" | "cancel" | "complete"
        ),
        "status" => matches!(
            value,
            "started" | "running" | "succeeded" | "failed" | "cancelled"
        ),
        "progress" => value
            .strip_suffix('%')
            .unwrap_or(value)
            .parse::<u8>()
            .is_ok_and(|progress| progress <= 100),
        _ => false,
    }
}

fn validate_id(value: &str, field: &'static str) -> Result<(), ParallelProofError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return Err(ParallelProofError::InvalidField(field));
    }
    Ok(())
}

fn envelope_key(job_id: &str) -> String {
    format!("job-{job_id}-envelope")
}

fn receipt_key(job_id: &str, sequence: u32) -> String {
    format!("job-{job_id}-receipt-{sequence:03}")
}

fn wake_ack_key(job_id: &str) -> String {
    format!("job-{job_id}-wake-ack")
}

fn log_key(job_id: &str, sequence: u32) -> String {
    format!("job-{job_id}-log-{sequence:03}")
}

fn artifact_key(job_id: &str) -> String {
    format!("job-{job_id}-artifact")
}

fn domain_digest<T: Serialize>(
    domain: &str,
    value: &T,
) -> Result<Sha256Digest, ParallelProofError> {
    let mut bytes = domain.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend(serde_json::to_vec(value)?);
    Ok(Sha256Digest::of_bytes(&bytes))
}

fn map_store_error(error: ImmutableStoreError) -> ParallelProofError {
    match error {
        ImmutableStoreError::InvalidRoot => ParallelProofError::InvalidField("canary job root"),
        ImmutableStoreError::UnsafePath(path) => {
            ParallelProofError::CorruptRecord(format!("unsafe canary job path {}", path.display()))
        }
        ImmutableStoreError::LimitExceeded { max, found } => ParallelProofError::LimitExceeded {
            field: "canary job record bytes",
            max,
            found,
        },
        ImmutableStoreError::Missing(key) => ParallelProofError::MissingRecord(key),
        ImmutableStoreError::Conflict(key) => ParallelProofError::ImmutableConflict(key),
        ImmutableStoreError::Io(error) => ParallelProofError::Io(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(seed: &str) -> Sha256Digest {
        Sha256Digest::of_bytes(seed.as_bytes())
    }

    fn job() -> ApprovedCanaryJob {
        ApprovedCanaryJob {
            schema_version: SCHEMA_VERSION,
            job_id: "canary-job-1".to_owned(),
            correlation_id: "canary-correlation-1".to_owned(),
            owner: CanaryJobOwner {
                controller_id: "shipyard-controller".to_owned(),
                controller_incarnation: "incarnation-7".to_owned(),
                approval_sha256: digest("approval"),
            },
            operation: ApprovedCanaryOperation::ParallelProofDistributedShadow {
                repository_id: 42,
                repository: "Generous-Corp/pulp".to_owned(),
                target: "macos".to_owned(),
                target_triple: "aarch64-apple-darwin".to_owned(),
                builder_host_id: "m3".to_owned(),
                worker_host_id: "m1".to_owned(),
                manifest_sha256: digest("manifest"),
                artifact_bytes_total: 1_024,
                invocation_authority_sha256: digest("authority"),
                adapter_executable_sha256: digest("adapter"),
            },
            approved_at_ms: 1_000,
            deadline_at_ms: 10_000,
            heartbeat_interval_ms: 100,
            heartbeat_timeout_ms: 500,
            max_heartbeat_receipts: 4,
            success: CanarySuccessPredicate {
                required_exit_code: 0,
                artifact_schema_version: 1,
                max_artifact_bytes: 4096,
            },
            cancellation: CanaryCancellationPolicy {
                grace_ms: 250,
                cancel_at_deadline: true,
            },
            wake: CanaryWakePredicate {
                on_success: true,
                on_actionable_failure: true,
            },
            logs: CanaryLogPolicy {
                segment_bytes: 1024,
                max_segments: 3,
            },
        }
    }

    fn process(job: &ApprovedCanaryJob) -> CanaryProcessTreeIdentity {
        let nonce = domain_digest(
            "shipyard.canary-job.launch-nonce.v1",
            &(job.digest().unwrap(), &job.owner.controller_incarnation),
        )
        .unwrap();
        let ApprovedCanaryOperation::ParallelProofDistributedShadow {
            adapter_executable_sha256,
            ..
        } = &job.operation;
        CanaryProcessTreeIdentity {
            pid: 42,
            tree_id: "pgrp:42".to_owned(),
            birth_token: "birth-1".to_owned(),
            launch_nonce_sha256: nonce,
            executable_sha256: adapter_executable_sha256.clone(),
            launched_at_ms: 1_100,
        }
    }

    fn response(job: &ApprovedCanaryJob) -> CanaryJobResponse {
        let launch_nonce_sha256 = domain_digest(
            "shipyard.canary-job.launch-nonce.v1",
            &(job.digest().unwrap(), &job.owner.controller_incarnation),
        )
        .unwrap();
        CanaryJobResponse {
            schema_version: job.success.artifact_schema_version,
            operation_sha256: job.operation.digest().unwrap(),
            job_sha256: job.digest().unwrap(),
            launch_nonce_sha256,
            observation: DistributedExecutionObservation {
                delivery: ArtifactDeliveryObservation {
                    mode: ArtifactDeliveryMode::FullTransfer,
                    artifact_bytes_total: 1_024,
                    artifact_bytes_reused: 0,
                    artifact_bytes_transferred: 1_024,
                    interruption: None,
                },
                setup_ms: 20,
                transfer_ms: 40,
                verification_ms: 10,
                dispatch_ms: 5,
                shard_execution_ms: 100,
                worker_active_ms: 180,
                submit_to_receipt_ms: 200,
                caches: Vec::new(),
            },
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        launches: u32,
        launch: Option<Result<CanaryProcessTreeIdentity, String>>,
        discovery: Option<Result<CanaryProcessObservation, String>>,
        observations: Vec<Result<CanaryProcessObservation, String>>,
        cancellation: Option<Result<CanaryCancellationObservation, String>>,
    }

    impl CanaryJobBackend for FakeBackend {
        fn launch(
            &mut self,
            _job: &ApprovedCanaryJob,
            _launch_nonce_sha256: &Sha256Digest,
            claimed_at_ms: u64,
        ) -> Result<CanaryProcessTreeIdentity, String> {
            assert_eq!(claimed_at_ms, 1_100);
            self.launches += 1;
            self.launch.take().expect("launch configured")
        }

        fn discover(
            &mut self,
            _job: &ApprovedCanaryJob,
            _launch_nonce_sha256: &Sha256Digest,
        ) -> Result<CanaryProcessObservation, String> {
            self.discovery.take().expect("discovery configured")
        }

        fn observe(
            &mut self,
            _job: &ApprovedCanaryJob,
            _process: &CanaryProcessTreeIdentity,
        ) -> Result<CanaryProcessObservation, String> {
            self.observations.remove(0)
        }

        fn cancel(
            &mut self,
            _job: &ApprovedCanaryJob,
            _process: &CanaryProcessTreeIdentity,
            grace_ms: u64,
        ) -> Result<CanaryCancellationObservation, String> {
            assert_eq!(grace_ms, 250);
            self.cancellation.take().expect("cancellation configured")
        }
    }

    #[test]
    fn exact_replay_never_redispatches() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let mut backend = FakeBackend {
            launch: Some(Ok(process(&job))),
            ..FakeBackend::default()
        };

        let first = launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        let replay = launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        assert!(first.launched);
        assert!(!replay.launched);
        assert_eq!(backend.launches, 1);
        assert_eq!(first.snapshot, replay.snapshot);
    }

    #[test]
    fn existing_launch_claim_is_the_only_spawn_authority() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        let (_, outcome) = store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        assert_eq!(outcome, StoreWriteOutcome::Created);
        let mut contender = FakeBackend::default();

        let result = launch_canary_job(&store, &job, 1_100, &mut contender).unwrap();

        assert!(!result.launched);
        assert_eq!(contender.launches, 0);
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Launching { .. }
        ));
    }

    #[test]
    fn durable_prelaunch_cancellation_wins_without_spawning() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let request = CanaryCancellationRequest {
            job_sha256: job.digest().unwrap(),
            controller_id: job.owner.controller_id.clone(),
            approval_sha256: job.owner.approval_sha256.clone(),
            requested_at_ms: 1_050,
        };
        store.request_cancel(&job.job_id, &request).unwrap();
        assert_eq!(
            store.request_cancel(&job.job_id, &request).unwrap(),
            StoreWriteOutcome::AlreadyPresent
        );
        let mut backend = FakeBackend::default();

        let result = launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        assert_eq!(backend.launches, 0);
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::CancelledBeforeLaunch,
                process: None,
                ..
            }
        ));
    }

    #[test]
    fn cancellation_during_launch_gap_is_applied_after_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        store
            .request_cancel(
                &job.job_id,
                &CanaryCancellationRequest {
                    job_sha256: job.digest().unwrap(),
                    controller_id: job.owner.controller_id.clone(),
                    approval_sha256: job.owner.approval_sha256.clone(),
                    requested_at_ms: 1_150,
                },
            )
            .unwrap();
        assert!(matches!(
            store.load(&job.job_id).unwrap().latest().receipt,
            CanaryJobReceiptState::CancellationRequestedBeforeIdentity { .. }
        ));
        let mut backend = FakeBackend {
            discovery: Some(Ok(CanaryProcessObservation::Alive(process(&job)))),
            cancellation: Some(Ok(CanaryCancellationObservation::Terminated)),
            ..FakeBackend::default()
        };

        let result = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();

        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn crash_after_spawn_is_discovered_not_redispatched() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("jobs");
        let store = CanaryJobStore::open(&root).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        drop(store);

        let reopened = CanaryJobStore::open(&root).unwrap();
        let expected = process(&job);
        let mut backend = FakeBackend {
            discovery: Some(Ok(CanaryProcessObservation::Alive(expected.clone()))),
            ..FakeBackend::default()
        };
        let reconciled = reconcile_canary_job(&reopened, &job.job_id, 1_200, &mut backend).unwrap();

        assert!(!reconciled.launched);
        assert_eq!(backend.launches, 0);
        assert!(matches!(
            reconciled.snapshot.latest().receipt,
            CanaryJobReceiptState::Running { ref process } if process == &expected
        ));
    }

    #[test]
    fn ambiguous_launch_error_remains_discoverable() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let expected = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Err("response lost after spawn token=secret".to_owned())),
            discovery: Some(Ok(CanaryProcessObservation::Alive(expected.clone()))),
            ..FakeBackend::default()
        };

        let ambiguous = launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        assert!(ambiguous.retryable_failure_sha256.is_some());
        assert!(matches!(
            ambiguous.snapshot.latest().receipt,
            CanaryJobReceiptState::Launching { .. }
        ));
        let recovered = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();
        assert!(matches!(
            recovered.snapshot.latest().receipt,
            CanaryJobReceiptState::Running { ref process } if process == &expected
        ));
    }

    #[test]
    fn future_dated_process_identity_is_never_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let mut future = process(&job);
        future.launched_at_ms = 9_000;
        let mut backend = FakeBackend {
            launch: Some(Ok(future)),
            ..FakeBackend::default()
        };

        assert!(matches!(
            launch_canary_job(&store, &job, 1_100, &mut backend),
            Err(ParallelProofError::CorruptRecord(message))
                if message == "canary process launch claim time"
        ));
        assert!(matches!(
            store.load(&job.job_id).unwrap().latest().receipt,
            CanaryJobReceiptState::Launching { .. }
        ));
    }

    #[test]
    fn missing_process_is_terminal_loss_not_success() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        let mut backend = FakeBackend {
            discovery: Some(Ok(CanaryProcessObservation::Missing)),
            ..FakeBackend::default()
        };

        let result = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();

        assert!(result.wake);
        assert_eq!(result.wake_receipt_sequence, Some(2));
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Lost,
                artifact: None,
                ..
            }
        ));
        let replay = reconcile_canary_job(&store, &job.job_id, 2_100, &mut backend).unwrap();
        assert!(replay.wake);
        assert_eq!(replay.wake_receipt_sequence, Some(2));
        store
            .acknowledge_wake(
                &job.job_id,
                &CanaryWakeAcknowledgement {
                    job_sha256: job.digest().unwrap(),
                    receipt_sha256: replay.snapshot.latest().digest().unwrap(),
                    controller_id: job.owner.controller_id.clone(),
                    approval_sha256: job.owner.approval_sha256.clone(),
                    acknowledged_at_ms: 2_200,
                },
            )
            .unwrap();
        let acknowledged = reconcile_canary_job(&store, &job.job_id, 2_300, &mut backend).unwrap();
        assert!(!acknowledged.wake);
        assert_eq!(acknowledged.wake_receipt_sequence, None);
    }

    #[test]
    fn transient_observation_error_preserves_retryable_custody() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![Err(
                "temporary transport outage with token=secret".to_owned()
            )],
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        let before = store.load(&job.job_id).unwrap();

        let retryable = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();

        assert!(retryable.retryable_failure_sha256.is_some());
        assert!(!format!("{retryable:?}").contains("temporary transport"));
        assert_eq!(store.load(&job.job_id).unwrap(), before);
        backend
            .observations
            .push(Ok(CanaryProcessObservation::Alive(process)));
        let retried = reconcile_canary_job(&store, &job.job_id, 1_300, &mut backend).unwrap();
        assert!(matches!(
            retried.snapshot.latest().receipt,
            CanaryJobReceiptState::Heartbeat { .. }
        ));
    }

    #[test]
    fn exit_requires_exact_artifact_predicate() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        let artifact = store.record_artifact(&job.job_id, &response(&job)).unwrap();
        backend
            .observations
            .push(Ok(CanaryProcessObservation::Exited {
                process,
                exit_code: Some(0),
                exited_at_ms: 1_500,
                artifact: Some(artifact),
            }));

        let result = reconcile_canary_job(&store, &job.job_id, 2_000, &mut backend).unwrap();

        assert!(result.wake);
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Succeeded,
                artifact: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn crash_before_running_receipt_can_recover_terminal_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        let artifact = store.record_artifact(&job.job_id, &response(&job)).unwrap();
        let mut backend = FakeBackend {
            discovery: Some(Ok(CanaryProcessObservation::Exited {
                process: process(&job),
                exit_code: Some(0),
                exited_at_ms: 1_500,
                artifact: Some(artifact),
            })),
            ..FakeBackend::default()
        };

        let recovered = reconcile_canary_job(&store, &job.job_id, 2_000, &mut backend).unwrap();

        assert!(recovered.wake);
        assert!(matches!(
            recovered.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Succeeded,
                ..
            }
        ));
    }

    #[test]
    fn exit_after_deadline_cannot_be_certified_as_success() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        let artifact = store.record_artifact(&job.job_id, &response(&job)).unwrap();
        backend
            .observations
            .push(Ok(CanaryProcessObservation::Exited {
                process,
                exit_code: Some(0),
                exited_at_ms: 10_001,
                artifact: Some(artifact),
            }));

        let result = reconcile_canary_job(&store, &job.job_id, 11_000, &mut backend).unwrap();

        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Failed,
                artifact: None,
                ..
            }
        ));
    }

    #[test]
    fn cancellation_cannot_erase_an_earlier_authenticated_exit() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        let artifact = store.record_artifact(&job.job_id, &response(&job)).unwrap();
        store
            .request_cancel(
                &job.job_id,
                &CanaryCancellationRequest {
                    job_sha256: job.digest().unwrap(),
                    controller_id: job.owner.controller_id.clone(),
                    approval_sha256: job.owner.approval_sha256.clone(),
                    requested_at_ms: 2_000,
                },
            )
            .unwrap();
        backend
            .observations
            .push(Ok(CanaryProcessObservation::Exited {
                process,
                exit_code: Some(0),
                exited_at_ms: 1_500,
                artifact: Some(artifact),
            }));

        let result = reconcile_canary_job(&store, &job.job_id, 2_100, &mut backend).unwrap();

        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Succeeded,
                ..
            }
        ));
    }

    #[test]
    fn zero_exit_without_artifact_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![Ok(CanaryProcessObservation::Exited {
                process,
                exit_code: Some(0),
                exited_at_ms: 1_500,
                artifact: None,
            })],
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        let result = reconcile_canary_job(&store, &job.job_id, 2_000, &mut backend).unwrap();
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Failed,
                artifact: None,
                ..
            }
        ));
    }

    #[test]
    fn malformed_typed_response_cannot_be_certified_as_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        assert!(matches!(
            store.record_artifact(&job.job_id, &response(&job)),
            Err(ParallelProofError::InvalidAttemptSequence(message))
                if message == "canary artifact requires launch custody"
        ));
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        let mut malformed = response(&job);
        malformed.observation.delivery.artifact_bytes_transferred = 1_023;

        assert!(matches!(
            store.record_artifact(&job.job_id, &malformed),
            Err(ParallelProofError::BindingMismatch(
                "canary typed response counters"
            ))
        ));
        assert!(matches!(
            store.artifacts.load(&artifact_key(&job.job_id)),
            Err(ImmutableStoreError::Missing(_))
        ));
    }

    #[test]
    fn artifact_from_same_operation_cannot_cross_job_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let first = job();
        let stale_response = response(&first);
        let mut second = first.clone();
        second.job_id = "canary-job-2".to_owned();
        second.correlation_id = "canary-correlation-2".to_owned();
        store.submit(&second).unwrap();
        let prepared = store.load(&second.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();

        assert!(matches!(
            store.record_artifact(&second.job_id, &stale_response),
            Err(ParallelProofError::BindingMismatch(
                "canary response identity"
            ))
        ));
    }

    #[test]
    fn deadline_cancellation_is_bounded_and_proven() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![Ok(CanaryProcessObservation::Alive(process))],
            cancellation: Some(Ok(CanaryCancellationObservation::Terminated)),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        let result = reconcile_canary_job(&store, &job.job_id, 10_001, &mut backend).unwrap();
        assert!(!result.wake);
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn stale_heartbeat_cancels_even_when_process_is_still_alive() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![Ok(CanaryProcessObservation::Alive(process))],
            cancellation: Some(Ok(CanaryCancellationObservation::Terminated)),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        let result = reconcile_canary_job(&store, &job.job_id, 1_601, &mut backend).unwrap();
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn rapid_poll_does_not_consume_heartbeat_budget() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![
                Ok(CanaryProcessObservation::Alive(process.clone())),
                Ok(CanaryProcessObservation::Alive(process)),
            ],
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        let early = reconcile_canary_job(&store, &job.job_id, 1_150, &mut backend).unwrap();
        assert!(matches!(
            early.snapshot.latest().receipt,
            CanaryJobReceiptState::Running { .. }
        ));
        assert_eq!(heartbeat_count(&early.snapshot).unwrap(), 0);
        let due = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();
        assert_eq!(heartbeat_count(&due.snapshot).unwrap(), 1);
    }

    #[test]
    fn heartbeat_limit_cancels_tree_before_terminal_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let mut job = job();
        job.max_heartbeat_receipts = 1;
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![
                Ok(CanaryProcessObservation::Alive(process.clone())),
                Ok(CanaryProcessObservation::Alive(process)),
            ],
            cancellation: Some(Ok(CanaryCancellationObservation::Terminated)),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();

        let result = reconcile_canary_job(&store, &job.job_id, 1_300, &mut backend).unwrap();

        assert!(backend.cancellation.is_none());
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::HeartbeatLimit,
                ..
            }
        ));
    }

    #[test]
    fn cancellation_missing_is_uncertain_and_actionable() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![Ok(CanaryProcessObservation::Alive(process))],
            cancellation: Some(Ok(CanaryCancellationObservation::Missing)),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        store
            .request_cancel(
                &job.job_id,
                &CanaryCancellationRequest {
                    job_sha256: job.digest().unwrap(),
                    controller_id: job.owner.controller_id.clone(),
                    approval_sha256: job.owner.approval_sha256.clone(),
                    requested_at_ms: 2_000,
                },
            )
            .unwrap();

        let result = reconcile_canary_job(&store, &job.job_id, 2_100, &mut backend).unwrap();
        assert!(result.wake);
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::CancellationUncertain,
                ..
            }
        ));
    }

    #[test]
    fn logs_are_redacted_immutable_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();

        store
            .record_log_segment(
                &job.job_id,
                0,
                b"phase=transfer\nauthorization: Bearer abc\ntoken=hunter2\npassword hunter3\nGH_PAT=ghp_abc\nAWS_ACCESS_KEY_ID=AKIA123\n-----BEGIN PRIVATE KEY-----\nurl=https://host/path?sig=abc\nok\x01\n",
            )
            .unwrap();
        let log = String::from_utf8(store.load_log_segment(&job.job_id, 0).unwrap()).unwrap();
        assert_eq!(log.matches("[REDACTED]").count(), 8);
        assert!(!log.contains("authorization"));
        assert!(!log.contains("token"));
        assert!(!log.contains("Bearer abc"));
        assert!(!log.contains("hunter2"));
        assert!(!log.contains("hunter3"));
        assert!(!log.contains("ghp_abc"));
        assert!(!log.contains("AKIA123"));
        assert!(!log.contains("PRIVATE KEY"));
        assert!(!log.contains("sig=abc"));
        assert!(!log.contains('\x01'));
        assert_eq!(
            store
                .record_log_segment(&job.job_id, 0, b"different")
                .unwrap_err()
                .to_string(),
            ParallelProofError::ImmutableConflict("job-canary-job-1-log-000".to_owned())
                .to_string()
        );
        assert!(matches!(
            store.record_log_segment(&job.job_id, 3, b"overflow"),
            Err(ParallelProofError::LimitExceeded { .. })
        ));
        assert!(matches!(
            redact_log(b"phase=transfer", b"phase=transfer".len()),
            Err(ParallelProofError::LimitExceeded {
                field: "redacted canary log segment bytes",
                ..
            })
        ));
    }

    #[test]
    fn contradictory_envelope_replay_fails_immutable() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let mut contradiction = job.clone();
        contradiction.deadline_at_ms += 1;

        assert!(matches!(
            store.submit(&contradiction),
            Err(ParallelProofError::ImmutableConflict(_))
        ));
    }

    #[test]
    fn partial_submission_recovers_after_owner_process_death() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("jobs");
        let store = CanaryJobStore::open(&root).unwrap();
        let job = job();
        store
            .records
            .put(
                &envelope_key(&job.job_id),
                &serde_json::to_vec(&job).unwrap(),
            )
            .unwrap();
        drop(store);

        let reopened_by_fresh_controller = CanaryJobStore::open(&root).unwrap();
        assert_eq!(
            reopened_by_fresh_controller.submit(&job).unwrap(),
            StoreWriteOutcome::Created
        );
        assert_eq!(
            reopened_by_fresh_controller
                .load(&job.job_id)
                .unwrap()
                .job
                .operation,
            job.operation
        );
    }

    #[test]
    fn malformed_heartbeat_cannot_skip_running_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap().latest().clone();
        let heartbeat = CanaryJobReceipt {
            schema_version: SCHEMA_VERSION,
            job_sha256: job.digest().unwrap(),
            sequence: 1,
            previous_receipt_sha256: Some(prepared.digest().unwrap()),
            receipt: CanaryJobReceiptState::Heartbeat {
                process: process(&job),
                observed_at_ms: 1_200,
            },
        };
        store.put_receipt(&job.job_id, &heartbeat).unwrap();

        assert!(matches!(
            store.load(&job.job_id),
            Err(ParallelProofError::CorruptRecord(message))
                if message == "canary heartbeat transition"
        ));
    }

    #[test]
    fn contradictory_durable_cancel_authority_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let request = CanaryCancellationRequest {
            job_sha256: job.digest().unwrap(),
            controller_id: "different-controller".to_owned(),
            approval_sha256: job.owner.approval_sha256.clone(),
            requested_at_ms: 2_000,
        };
        assert!(matches!(
            store.request_cancel(&job.job_id, &request),
            Err(ParallelProofError::AuthenticationFailed)
        ));
        assert!(matches!(
            store.load(&job.job_id).unwrap().latest().receipt,
            CanaryJobReceiptState::Prepared { .. }
        ));
    }
}
