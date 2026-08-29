//! Job and target-result domain types.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default age, in seconds, past which a `Running` job whose worker has gone
/// silent is treated as abandoned by a dead worker. Mirrors the host-pool
/// lease-staleness convention (`host_pool::DEFAULT_LEASE_STALE_SECONDS`); kept
/// as an independent constant so the core job domain does not depend on the
/// resource subsystem. A live worker heartbeats roughly every 15s, so 180s of
/// silence is well past any healthy gap.
pub const DEFAULT_RUNNING_JOB_STALE_SECONDS: i64 = 180;

/// Job scheduling priority.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// Low priority.
    Low,
    /// Normal priority.
    Normal,
    /// High priority.
    High,
}

impl Priority {
    /// Numeric sort value matching Python Shipyard.
    #[must_use]
    pub fn value(self) -> i32 {
        match self {
            Self::Low => 10,
            Self::Normal => 50,
            Self::High => 100,
        }
    }
}

/// Validation thoroughness mode.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ValidationMode {
    /// Full validation.
    Full,
    /// Smoke validation.
    Smoke,
}

/// Durable execution request kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    /// `shipyard run` validation.
    Run,
    /// `shipyard ship` validation.
    Ship,
}

/// Job lifecycle state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Waiting to run.
    Pending,
    /// Currently running.
    Running,
    /// Completed with terminal target results.
    Completed,
    /// Cancelled before completion.
    Cancelled,
}

/// Machine-readable authority for a cancellation disposition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationCause {
    /// An authenticated observer proved that the exact submitted PR head merged.
    AlreadyMerged,
}

/// Authenticated identity bound to a typed cancellation cause.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CancellationProof {
    /// Typed cause established by the trusted observer.
    pub cause: CancellationCause,
    /// Canonical repository slug observed by the provider client.
    pub repository: String,
    /// Pull-request number observed by the provider client.
    pub pull_request: u64,
    /// Exact merged head SHA matching the queued immutable request.
    pub head_sha: String,
}

/// Target result state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetStatus {
    /// Waiting to run.
    Pending,
    /// Currently running.
    Running,
    /// Validation passed.
    Pass,
    /// Validation failed.
    Fail,
    /// Executor or environment error.
    Error,
    /// Target could not be reached.
    Unreachable,
    /// Target was cancelled.
    Cancelled,
}

impl TargetStatus {
    /// Whether this status is terminal.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Pass | Self::Fail | Self::Error | Self::Unreachable | Self::Cancelled
        )
    }
}

/// Outcome of validating one target.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TargetResult {
    /// Target name.
    #[serde(rename = "target")]
    pub target_name: String,
    /// Platform label.
    pub platform: String,
    /// Result status.
    pub status: TargetStatus,
    /// Backend label.
    pub backend: String,
    /// Git HEAD observed in the execution checkout before validation began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_head_sha: Option<String>,
    /// Git tree observed in the execution checkout before validation began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tree_sha: Option<String>,
    /// Whether the execution checkout was clean before validation began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_checkout_clean: Option<bool>,
    /// Whether validation began without resume or prepared-state stage reuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_execution: Option<bool>,
    /// Duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    /// Start timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// Completion timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Local log path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// Current or failed phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Last output timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_at: Option<DateTime<Utc>>,
    /// Last heartbeat timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Quiet duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_for_secs: Option<f64>,
    /// Liveness label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness: Option<String>,
    /// Primary backend for failover results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_backend: Option<String>,
    /// Failover reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover_reason: Option<String>,
    /// Provider label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Runner profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_profile: Option<String>,
    /// Error detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Contract markers observed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contract_markers_seen: Vec<String>,
    /// Contract markers missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contract_markers_missing: Vec<String>,
    /// Contract violation message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_violation: Option<String>,
    /// Failure classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    /// Ancestor SHA reused for evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_from: Option<String>,
    /// GitHub Actions workflow run ID (cloud backend only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_run_id: Option<u64>,
    /// GitHub Actions job database ID for the failing job (cloud backend only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_job_id: Option<u64>,
    /// GitHub Actions job display name (cloud backend only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_job_name: Option<String>,
    /// GitHub Actions job HTML URL (cloud backend only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_job_url: Option<String>,
    /// Name of the failing step inside the failing job (cloud backend only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_failed_step: Option<String>,
    /// Per-target failure parser selection from `.shipyard/config.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_parser: Option<String>,
    /// Scheduler-owned deferral reason. A result with this set must not be
    /// treated as a terminal validation outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler_defer_reason: Option<String>,
}

impl TargetResult {
    /// Construct a target result with required fields.
    #[must_use]
    pub fn new(
        target_name: impl Into<String>,
        platform: impl Into<String>,
        status: TargetStatus,
        backend: impl Into<String>,
    ) -> Self {
        Self {
            target_name: target_name.into(),
            platform: platform.into(),
            status,
            backend: backend.into(),
            source_head_sha: None,
            source_tree_sha: None,
            source_checkout_clean: None,
            full_execution: None,
            duration_secs: None,
            started_at: None,
            completed_at: None,
            log_path: None,
            phase: None,
            last_output_at: None,
            last_heartbeat_at: None,
            quiet_for_secs: None,
            liveness: None,
            primary_backend: None,
            failover_reason: None,
            provider: None,
            runner_profile: None,
            error_message: None,
            contract_markers_seen: Vec::new(),
            contract_markers_missing: Vec::new(),
            contract_violation: None,
            failure_class: None,
            reused_from: None,
            cloud_run_id: None,
            cloud_job_id: None,
            cloud_job_name: None,
            cloud_job_url: None,
            cloud_failed_step: None,
            failure_parser: None,
            scheduler_defer_reason: None,
        }
    }

    /// Whether the target passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == TargetStatus::Pass
    }

    /// Whether this result is terminal.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// Whether this result is a scheduler-owned deferral instead of a final
    /// validation outcome.
    #[must_use]
    pub fn is_scheduler_deferred(&self) -> bool {
        self.scheduler_defer_reason.is_some()
    }

    /// Convert to Python-compatible JSON value.
    #[must_use]
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("TargetResult must serialize")
    }
}

/// Validation job across one or more targets.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Job {
    /// Job identifier.
    pub id: String,
    /// Commit SHA.
    pub sha: String,
    /// Branch name.
    pub branch: String,
    /// Validation mode.
    pub mode: ValidationMode,
    /// Durable request kind. Legacy jobs may not have this yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<JobKind>,
    /// Stable workload identity used to keep queue supersedence repo-neutral.
    /// Legacy jobs may not have this yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_scope: Option<String>,
    /// Target names.
    #[serde(rename = "targets")]
    pub target_names: Vec<String>,
    /// Scheduling priority.
    pub priority: Priority,
    /// Lifecycle status.
    pub status: JobStatus,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Start timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// Completion timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Optional reason when a job is cancelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_reason: Option<String>,
    /// Typed authenticated cancellation authority. Legacy and manual cancels omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_proof: Option<CancellationProof>,
    /// Timestamp when cancellation was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_requested_at: Option<DateTime<Utc>>,
    /// Scheduler deferral reason when a running job is returned to pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler_defer_reason: Option<String>,
    /// Number of scheduler-owned deferrals for this job.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub scheduler_defer_count: u32,
    /// Earliest time the scheduler should retry this job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler_defer_until: Option<DateTime<Utc>>,
    /// Resource claims held or attempted by the queue scheduler.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_claims: Vec<String>,
    /// Results keyed by target name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub results: BTreeMap<String, TargetResult>,
}

impl Job {
    /// Create a new pending job.
    #[must_use]
    pub fn create(
        sha: impl Into<String>,
        branch: impl Into<String>,
        target_names: Vec<String>,
        mode: ValidationMode,
        priority: Priority,
    ) -> Self {
        let sha = sha.into();
        let branch = branch.into();
        let created_at = Utc::now();
        let id = generate_id(created_at, &sha, &branch, &target_names);
        Self {
            id,
            sha,
            branch,
            mode,
            target_names,
            priority,
            status: JobStatus::Pending,
            created_at,
            started_at: None,
            completed_at: None,
            cancellation_reason: None,
            cancellation_proof: None,
            cancel_requested_at: None,
            scheduler_defer_reason: None,
            scheduler_defer_count: 0,
            scheduler_defer_until: None,
            resource_claims: Vec::new(),
            results: BTreeMap::new(),
            kind: None,
            workload_scope: None,
        }
    }

    /// Transition from pending to running.
    pub fn start(&self) -> Result<Self, JobTransitionError> {
        if self.status != JobStatus::Pending {
            return Err(JobTransitionError::InvalidStart(self.status));
        }
        let mut next = self.clone();
        next.status = JobStatus::Running;
        next.started_at = Some(Utc::now());
        next.scheduler_defer_reason = None;
        next.scheduler_defer_until = None;
        Ok(next)
    }

    /// Transition from running to completed.
    pub fn complete(&self) -> Result<Self, JobTransitionError> {
        if self.status != JobStatus::Running {
            return Err(JobTransitionError::InvalidComplete(self.status));
        }
        let mut next = self.clone();
        next.status = JobStatus::Completed;
        next.completed_at = Some(Utc::now());
        Ok(next)
    }

    /// Cancel any non-terminal job.
    pub fn cancel(&self) -> Result<Self, JobTransitionError> {
        self.cancel_with_reason(None)
    }

    /// Cancel any non-terminal job with an optional reason.
    pub fn cancel_with_reason(&self, reason: Option<String>) -> Result<Self, JobTransitionError> {
        self.cancel_with_reason_and_proof(reason, None)
    }

    pub(crate) fn cancel_with_reason_and_proof(
        &self,
        reason: Option<String>,
        proof: Option<CancellationProof>,
    ) -> Result<Self, JobTransitionError> {
        if matches!(self.status, JobStatus::Completed | JobStatus::Cancelled) {
            return Err(JobTransitionError::InvalidCancel(self.status));
        }
        let mut next = self.clone();
        next.status = JobStatus::Cancelled;
        next.completed_at = Some(Utc::now());
        next.cancellation_reason = reason;
        next.cancellation_proof = proof;
        next.cancel_requested_at = self.cancel_requested_at.or(next.completed_at);
        Ok(next)
    }

    /// Request cancellation without releasing a running job's resource claims.
    /// Pending work can terminate immediately; running work remains running
    /// until its exact owner has stopped the process tree and acknowledges the
    /// request with [`Self::cancel_with_reason`].
    pub fn request_cancel_with_reason(
        &self,
        reason: Option<String>,
    ) -> Result<Self, JobTransitionError> {
        self.request_cancel_with_reason_and_proof(reason, None)
    }

    pub(crate) fn request_cancel_with_reason_and_proof(
        &self,
        reason: Option<String>,
        proof: Option<CancellationProof>,
    ) -> Result<Self, JobTransitionError> {
        if self.status == JobStatus::Pending {
            return self.cancel_with_reason_and_proof(reason, proof);
        }
        if self.status != JobStatus::Running {
            return Err(JobTransitionError::InvalidCancel(self.status));
        }
        if self
            .cancellation_proof
            .as_ref()
            .zip(proof.as_ref())
            .is_some_and(|(existing, replacement)| existing != replacement)
        {
            return Err(JobTransitionError::ConflictingCancellationProof);
        }
        let mut next = self.clone();
        let proof_upgrade = self.cancellation_proof.is_none() && proof.is_some();
        next.cancellation_reason = if proof_upgrade {
            reason.or_else(|| self.cancellation_reason.clone())
        } else {
            self.cancellation_reason.clone().or(reason)
        };
        // Repeated controller/operator requests are idempotent with respect
        // to the original cancellation boundary.  In particular, a later
        // untyped manual request must not erase an authenticated
        // already-merged proof or make a long-stale orphan appear freshly
        // cancelled on every invocation.
        next.cancellation_proof = proof.or_else(|| self.cancellation_proof.clone());
        next.cancel_requested_at = self.cancel_requested_at.or_else(|| Some(Utc::now()));
        Ok(next)
    }

    /// Return a copy tagged with the durable request kind.
    #[must_use]
    pub fn with_kind(&self, kind: JobKind) -> Self {
        let mut next = self.clone();
        next.kind = Some(kind);
        next
    }

    /// Return a copy tagged with the durable workload identity.
    #[must_use]
    pub fn with_workload_scope(&self, workload_scope: impl Into<String>) -> Self {
        let mut next = self.clone();
        next.workload_scope = Some(workload_scope.into());
        next
    }

    /// Return a copy with scheduler resource-claim debug labels.
    #[must_use]
    pub fn with_resource_claims(&self, resource_claims: Vec<String>) -> Self {
        let mut next = self.clone();
        next.resource_claims = resource_claims;
        next
    }

    /// Return a copy with a different priority.
    #[must_use]
    pub fn with_priority(&self, priority: Priority) -> Self {
        let mut next = self.clone();
        next.priority = priority;
        next
    }

    /// Return a copy with an updated target result.
    #[must_use]
    pub fn with_result(&self, result: TargetResult) -> Self {
        let mut next = self.clone();
        next.results.insert(result.target_name.clone(), result);
        next
    }

    /// Return a running job to pending after a scheduler-owned transient
    /// deferral. Non-terminal target results are cleared so they can be retried.
    pub fn defer_for_scheduler(
        &self,
        reason: impl Into<String>,
        defer_until: Option<DateTime<Utc>>,
    ) -> Result<Self, JobTransitionError> {
        if self.status != JobStatus::Running {
            return Err(JobTransitionError::InvalidDefer(self.status));
        }
        let mut next = self.clone();
        next.status = JobStatus::Pending;
        next.started_at = None;
        next.completed_at = None;
        next.scheduler_defer_reason = Some(reason.into());
        next.scheduler_defer_count = next.scheduler_defer_count.saturating_add(1);
        next.scheduler_defer_until = defer_until;
        next.results.retain(|_, result| result.is_terminal());
        Ok(next)
    }

    /// Whether all targets passed and the job completed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == JobStatus::Completed
            && self.results.len() == self.target_names.len()
            && self.results.values().all(TargetResult::passed)
    }

    /// Whether every target has a terminal result.
    #[must_use]
    pub fn all_targets_terminal(&self) -> bool {
        self.results.len() == self.target_names.len()
            && self.results.values().all(TargetResult::is_terminal)
    }

    /// Most recent liveness signal for the job: the newest of every per-target
    /// heartbeat and `started_at`. Returns `None` for a job that has neither
    /// (e.g. a pending job), which callers treat as "no liveness anchor".
    ///
    /// `started_at` is included in the max — not just used as a fallback —
    /// because `defer_for_scheduler` retains terminal target results, so a
    /// requeued-and-restarted job can carry an old heartbeat that predates its
    /// new `started_at`. Taking the max keeps that freshly restarted live job
    /// from being misread as stale.
    #[must_use]
    pub fn last_liveness_at(&self) -> Option<DateTime<Utc>> {
        self.results
            .values()
            .filter_map(|result| result.last_heartbeat_at)
            .chain(self.started_at)
            .max()
    }

    /// Whether this is a `Running` job whose worker appears dead: its freshest
    /// liveness signal is older than `stale_after`. Only ever true for
    /// `Running` jobs; a job with no liveness anchor or a future-dated anchor
    /// (clock skew) is conservatively treated as not stale.
    ///
    /// This is heartbeat-age based on purpose — unlike the startup recovery in
    /// `Queue::recover_stale_running_jobs_for_drain`, which completes every
    /// running job unconditionally because it only runs when no worker can have
    /// survived. This predicate is safe to consult while workers may be live.
    #[must_use]
    pub fn is_stale_running(&self, now: DateTime<Utc>, stale_after: chrono::Duration) -> bool {
        if self.status != JobStatus::Running || stale_after <= chrono::Duration::zero() {
            return false;
        }
        match self.last_liveness_at() {
            Some(anchor) => now.signed_duration_since(anchor) >= stale_after,
            None => false,
        }
    }

    /// Convert to Python-compatible JSON value.
    #[must_use]
    pub fn to_json_value(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("Job must serialize");
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "overall".to_owned(),
                serde_json::Value::String(if self.passed() {
                    "pass".to_owned()
                } else if self.status == JobStatus::Completed {
                    "fail".to_owned()
                } else {
                    status_str(self.status).to_owned()
                }),
            );
        }
        value
    }
}

/// Invalid job transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobTransitionError {
    /// Cannot start from this status.
    InvalidStart(JobStatus),
    /// Cannot complete from this status.
    InvalidComplete(JobStatus),
    /// Cannot cancel from this status.
    InvalidCancel(JobStatus),
    /// A later controller tried to replace authenticated cancellation authority.
    ConflictingCancellationProof,
    /// Cannot defer from this status.
    InvalidDefer(JobStatus),
}

impl std::fmt::Display for JobTransitionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidStart(status) => write!(formatter, "cannot start job in state {status:?}"),
            Self::InvalidComplete(status) => {
                write!(formatter, "cannot complete job in state {status:?}")
            }
            Self::InvalidCancel(status) => {
                write!(formatter, "cannot cancel job in state {status:?}")
            }
            Self::ConflictingCancellationProof => {
                write!(
                    formatter,
                    "cancellation proof contradicts existing authority"
                )
            }
            Self::InvalidDefer(status) => write!(formatter, "cannot defer job in state {status:?}"),
        }
    }
}

impl std::error::Error for JobTransitionError {}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn generate_id(
    created_at: DateTime<Utc>,
    sha: &str,
    branch: &str,
    target_names: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(created_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
    hasher.update([0]);
    hasher.update(sha.as_bytes());
    hasher.update([0]);
    hasher.update(branch.as_bytes());
    hasher.update([0]);
    for target in target_names {
        hasher.update(target.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    format!(
        "sy-{}-{}",
        created_at.format("%Y%m%d"),
        hex::encode(&digest[..3])
    )
}

fn status_str(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Pending => "pending",
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests;
