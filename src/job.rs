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
        if matches!(self.status, JobStatus::Completed | JobStatus::Cancelled) {
            return Err(JobTransitionError::InvalidCancel(self.status));
        }
        let mut next = self.clone();
        next.status = JobStatus::Cancelled;
        next.completed_at = Some(Utc::now());
        next.cancellation_reason = reason;
        next.cancel_requested_at = next.completed_at;
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
mod tests {
    use serde_json::Value;

    use super::{
        Job, JobStatus, JobTransitionError, Priority, TargetResult, TargetStatus, ValidationMode,
    };

    fn job() -> Job {
        Job::create(
            "abc123",
            "feat/x",
            vec!["mac".to_owned(), "linux".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        )
    }

    #[test]
    fn priority_values_match_python_enum() {
        assert_eq!(Priority::Low.value(), 10);
        assert_eq!(Priority::Normal.value(), 50);
        assert_eq!(Priority::High.value(), 100);
    }

    #[test]
    fn target_status_terminal_matches_python_contract() {
        assert!(!TargetStatus::Pending.is_terminal());
        assert!(!TargetStatus::Running.is_terminal());
        for status in [
            TargetStatus::Pass,
            TargetStatus::Fail,
            TargetStatus::Error,
            TargetStatus::Unreachable,
            TargetStatus::Cancelled,
        ] {
            assert!(status.is_terminal(), "{status:?}");
        }
    }

    #[test]
    fn target_result_serializes_python_shape() {
        let mut result = TargetResult::new("mac", "macos", TargetStatus::Pass, "local");
        result.duration_secs = Some(1.24);
        result.contract_markers_seen = vec!["SMOKE".to_owned()];
        let value = result.to_json_value();
        assert_eq!(value["target"], "mac");
        assert_eq!(value["platform"], "macos");
        assert_eq!(value["status"], "pass");
        assert_eq!(value["backend"], "local");
        assert_eq!(value["contract_markers_seen"][0], "SMOKE");
        assert!(value.get("error_message").is_none());
    }

    #[test]
    fn job_create_sets_pending_state_and_id_shape() {
        let job = job();
        assert!(job.id.starts_with("sy-"));
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.mode, ValidationMode::Full);
        assert_eq!(job.priority, Priority::Normal);
        assert_eq!(job.target_names, vec!["mac", "linux"]);
    }

    fn running_with_heartbeat(
        now: chrono::DateTime<chrono::Utc>,
        started_offset_secs: i64,
        heartbeat_offset_secs: Option<i64>,
    ) -> Job {
        let mut running = job().start().expect("pending job starts");
        running.started_at = Some(now - chrono::Duration::seconds(started_offset_secs));
        if let Some(offset) = heartbeat_offset_secs {
            let mut result = TargetResult::new("mac", "macos", TargetStatus::Running, "local");
            result.last_heartbeat_at = Some(now - chrono::Duration::seconds(offset));
            running.results.insert("mac".to_owned(), result);
        }
        running
    }

    #[test]
    fn last_liveness_at_prefers_newest_heartbeat_then_started_at() {
        let now = chrono::Utc::now();
        let mut running = running_with_heartbeat(now, 600, None);
        // No heartbeats yet -> falls back to started_at.
        assert_eq!(running.last_liveness_at(), running.started_at);

        let mut older = TargetResult::new("mac", "macos", TargetStatus::Running, "local");
        older.last_heartbeat_at = Some(now - chrono::Duration::seconds(300));
        let mut newer = TargetResult::new("linux", "linux", TargetStatus::Running, "local");
        newer.last_heartbeat_at = Some(now - chrono::Duration::seconds(30));
        running.results.insert("mac".to_owned(), older);
        running.results.insert("linux".to_owned(), newer);
        // Freshest heartbeat wins over started_at and the older heartbeat.
        assert_eq!(
            running.last_liveness_at(),
            Some(now - chrono::Duration::seconds(30))
        );
    }

    #[test]
    fn last_liveness_at_includes_started_at_over_stale_retained_heartbeat() {
        let now = chrono::Utc::now();
        // A requeued-then-restarted job: a fresh `started_at` but an old
        // terminal-result heartbeat retained from the prior run. Liveness is the
        // newer `started_at`, so the restarted live job is not stale.
        let restarted = running_with_heartbeat(now, 5, Some(1000));
        assert_eq!(restarted.last_liveness_at(), restarted.started_at);
        assert!(!restarted.is_stale_running(now, chrono::Duration::seconds(180)));
    }

    #[test]
    fn is_stale_running_only_for_running_jobs() {
        let now = chrono::Utc::now();
        let stale_after = chrono::Duration::seconds(180);
        // Pending job: never stale, even though created_at is "now".
        assert!(!job().is_stale_running(now, stale_after));
        // Cancelled job: never stale.
        let cancelled = job().cancel().expect("pending job cancels");
        assert!(!cancelled.is_stale_running(now, stale_after));
        // Completed job: never stale.
        let completed = job()
            .start()
            .and_then(|running| running.complete())
            .expect("pending -> running -> completed");
        assert!(!completed.is_stale_running(now, stale_after));
    }

    #[test]
    fn is_stale_running_uses_heartbeat_age_threshold() {
        let now = chrono::Utc::now();
        let stale_after = chrono::Duration::seconds(180);
        // Ancient started_at but a fresh heartbeat -> not stale.
        let fresh = running_with_heartbeat(now, 1000, Some(30));
        assert!(!fresh.is_stale_running(now, stale_after));
        // Just under the threshold -> not stale.
        let almost = running_with_heartbeat(now, 1000, Some(179));
        assert!(!almost.is_stale_running(now, stale_after));
        // At/over the threshold -> stale.
        let stale = running_with_heartbeat(now, 1000, Some(200));
        assert!(stale.is_stale_running(now, stale_after));
    }

    #[test]
    fn is_stale_running_uses_started_at_when_no_heartbeat() {
        let now = chrono::Utc::now();
        let stale_after = chrono::Duration::seconds(180);
        // Running, never heartbeat, started long ago -> stale via started_at.
        let stale = running_with_heartbeat(now, 1000, None);
        assert!(stale.is_stale_running(now, stale_after));
        // Running, never heartbeat, started recently -> not stale.
        let fresh = running_with_heartbeat(now, 5, None);
        assert!(!fresh.is_stale_running(now, stale_after));
    }

    #[test]
    fn is_stale_running_handles_no_anchor_skew_and_zero_threshold() {
        let now = chrono::Utc::now();
        let stale_after = chrono::Duration::seconds(180);
        // No anchor at all (no started_at, no heartbeat) -> not stale.
        let mut no_anchor = running_with_heartbeat(now, 1000, None);
        no_anchor.started_at = None;
        assert!(!no_anchor.is_stale_running(now, stale_after));
        // Future-dated heartbeat (clock skew) -> not stale.
        let skewed = running_with_heartbeat(now, 0, Some(-60));
        assert!(!skewed.is_stale_running(now, stale_after));
        // Zero/negative threshold -> never stale.
        let ancient = running_with_heartbeat(now, 10_000, Some(10_000));
        assert!(!ancient.is_stale_running(now, chrono::Duration::zero()));
    }

    #[test]
    fn job_transitions_are_immutable() {
        let pending = job();
        let running = pending.start().expect("start");
        assert_eq!(pending.status, JobStatus::Pending);
        assert_eq!(running.status, JobStatus::Running);
        assert!(running.started_at.is_some());

        let completed = running.complete().expect("complete");
        assert_eq!(completed.status, JobStatus::Completed);
        assert!(completed.completed_at.is_some());
    }

    #[test]
    fn invalid_transitions_return_errors() {
        let pending = job();
        assert_eq!(
            pending.complete().expect_err("cannot complete pending"),
            JobTransitionError::InvalidComplete(JobStatus::Pending)
        );

        let completed = pending
            .start()
            .expect("start")
            .complete()
            .expect("complete");
        assert_eq!(
            completed.start().expect_err("cannot restart completed"),
            JobTransitionError::InvalidStart(JobStatus::Completed)
        );
        assert_eq!(
            completed.cancel().expect_err("cannot cancel completed"),
            JobTransitionError::InvalidCancel(JobStatus::Completed)
        );
    }

    #[test]
    fn cancel_sets_terminal_cancelled_state() {
        let cancelled = job().cancel().expect("cancel");
        assert_eq!(cancelled.status, JobStatus::Cancelled);
        assert!(cancelled.completed_at.is_some());
        assert!(cancelled.cancel_requested_at.is_some());
    }

    #[test]
    fn with_priority_and_result_return_updated_copies() {
        let job = job();
        let high = job.with_priority(Priority::High);
        assert_eq!(job.priority, Priority::Normal);
        assert_eq!(high.priority, Priority::High);

        let result = TargetResult::new("mac", "macos", TargetStatus::Pass, "local");
        let updated = job.with_result(result);
        assert!(job.results.is_empty());
        assert_eq!(updated.results["mac"].status, TargetStatus::Pass);
    }

    #[test]
    fn passed_requires_completed_and_all_targets_passed() {
        let running = job().start().expect("start");
        let with_mac = running.with_result(TargetResult::new(
            "mac",
            "macos",
            TargetStatus::Pass,
            "local",
        ));
        assert!(!with_mac.passed());
        assert!(!with_mac.all_targets_terminal());

        let with_linux = with_mac.with_result(TargetResult::new(
            "linux",
            "linux",
            TargetStatus::Pass,
            "ssh",
        ));
        assert!(with_linux.all_targets_terminal());
        assert!(!with_linux.passed());

        let completed = with_linux.complete().expect("complete");
        assert!(completed.passed());
    }

    #[test]
    fn failed_target_is_terminal_but_not_passed() {
        let running = job().start().expect("start");
        let with_results = running
            .with_result(TargetResult::new(
                "mac",
                "macos",
                TargetStatus::Pass,
                "local",
            ))
            .with_result(TargetResult::new(
                "linux",
                "linux",
                TargetStatus::Fail,
                "ssh",
            ));
        assert!(with_results.all_targets_terminal());
        assert!(!with_results.complete().expect("complete").passed());
    }

    #[test]
    fn job_serializes_status_and_results() {
        let running = job().start().expect("start").with_result(TargetResult::new(
            "mac",
            "macos",
            TargetStatus::Pass,
            "local",
        ));
        let value = running.to_json_value();
        assert_eq!(value["status"], "running");
        assert_eq!(value["overall"], "running");
        assert_eq!(value["priority"], "normal");
        assert_eq!(value["results"]["mac"]["status"], "pass");
        assert_eq!(
            value["targets"],
            Value::Array(vec!["mac".into(), "linux".into()])
        );
    }

    #[test]
    fn legacy_job_without_kind_deserializes() {
        let value = serde_json::json!({
            "id": "sy-legacy",
            "sha": "abc123",
            "branch": "feat/x",
            "mode": "full",
            "targets": ["mac"],
            "priority": "normal",
            "status": "pending",
            "created_at": "2026-05-26T00:00:00Z"
        });

        let job: Job = serde_json::from_value(value).expect("legacy job");

        assert_eq!(job.kind, None);
        assert_eq!(job.workload_scope, None);
        assert!(job.resource_claims.is_empty());
        assert_eq!(job.cancel_requested_at, None);
    }
}
