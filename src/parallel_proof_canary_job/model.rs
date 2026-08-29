
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
        /// Digest of the exact private invocation bytes admitted by the controller.
        request_sha256: Sha256Digest,
        /// Digest of the immutable release/build artifact authority.
        release_sha256: Sha256Digest,
        /// Authenticated builder session generation, fencing reconnects.
        builder_session_generation: u64,
        /// Authenticated worker session generation, fencing reconnects.
        worker_session_generation: u64,
        /// Digest of the exact required cache-generation authority.
        cache_authority_sha256: Sha256Digest,
        /// Digest of both authenticated staging roots, classes, and reserves.
        storage_authority_sha256: Sha256Digest,
        /// Exact manifest-bound encoded artifact size.
        artifact_bytes_total: u64,
        /// Digest of trusted machine-global invocation authority.
        invocation_authority_sha256: Sha256Digest,
        /// Digest of the exact provider adapter executable.
        adapter_executable_sha256: Sha256Digest,
        /// Digest of the exact Shipyard hidden-worker binary.
        worker_executable_sha256: Sha256Digest,
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
            builder_session_generation,
            worker_session_generation,
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
        if builder_host_id == worker_host_id
            || *builder_session_generation == 0
            || *worker_session_generation == 0
            || *artifact_bytes_total == 0
        {
            return Err(ParallelProofError::InvalidField(
                "canary job distinct hosts",
            ));
        }
        Ok(())
    }

    /// Digest every closed operation input and authority binding.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
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

/// Exact existing native continuation authority selected before job admission.
///
/// This record deliberately contains no route construction inputs. The native
/// work ledger must already contain the exact work, staged route, and protected
/// launch profile. Canary completion may only consume that authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryNativeContinuationBinding {
    /// Binding schema, independent of the canary job and receipt schemas.
    pub schema_version: u32,
    /// Exact existing native work item.
    pub work_item_id: String,
    /// Native work generation observed at canary admission.
    pub work_generation: u64,
    /// Exact native owner generation.
    pub owner_generation: u64,
    /// Existing staged route selected by native publication.
    pub route_ref: String,
    /// Existing protected launch-profile reference.
    pub profile_ref: String,
    /// Digest of the exact protected profile bytes used as the wake payload.
    pub payload_digest: String,
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
    /// Existing native continuation authority. Required by schema v2 jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_continuation: Option<CanaryNativeContinuationBinding>,
    /// Redacted rotated log bounds.
    pub logs: CanaryLogPolicy,
}

impl ApprovedCanaryJob {
    /// Validate every bound and timing relationship.
    pub fn validate(&self) -> Result<(), ParallelProofError> {
        if !matches!(
            self.schema_version,
            LEGACY_JOB_SCHEMA_VERSION | CURRENT_JOB_SCHEMA_VERSION
        ) {
            return Err(ParallelProofError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if (self.schema_version == CURRENT_JOB_SCHEMA_VERSION) != self.native_continuation.is_some()
        {
            return Err(ParallelProofError::InvalidField(
                "canary native continuation binding",
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
        if let Some(binding) = &self.native_continuation
            && (binding.schema_version != 1
                || binding.work_generation == 0
                || binding.owner_generation == 0
                || binding.work_item_id.is_empty()
                || binding.route_ref.is_empty()
                || binding.profile_ref.is_empty()
                || binding.payload_digest.len() != 64
                || !binding
                    .payload_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        {
            return Err(ParallelProofError::InvalidField(
                "canary native continuation binding",
            ));
        }
        self.operation.digest()?;
        Ok(())
    }

    /// Digest of every immutable execution input and predicate.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        self.validate()?;
        domain_digest(
            if self.schema_version == LEGACY_JOB_SCHEMA_VERSION {
                "shipyard.canary-job.envelope.v1"
            } else {
                "shipyard.canary-job.envelope.v2"
            },
            self,
        )
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
    /// Digest of the OS process start identity captured after spawn.
    pub os_start_identity_sha256: Sha256Digest,
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
            worker_executable_sha256,
            ..
        } = &job.operation;
        if self.pid == 0
            || self.launched_at_ms < job.approved_at_ms
            || self.launched_at_ms >= job.deadline_at_ms
            || self.launch_nonce_sha256 != *expected_nonce
            || self.executable_sha256 != *worker_executable_sha256
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
    /// Deterministic native outbox identity proven before acknowledgement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_wake_id: Option<String>,
    /// Digest of the exact native delivery receipt and terminal receipt link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_delivery_sha256: Option<Sha256Digest>,
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
        if self.schema_version != RECEIPT_SCHEMA_VERSION
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
