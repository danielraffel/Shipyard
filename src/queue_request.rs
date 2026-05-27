//! Durable queued execution request and outcome stores.
//!
//! Queue request snapshots are owned by the queue layer instead of making
//! executor runtime structs a serde compatibility contract.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::executor::cloud::CloudTargetConfig;
use crate::executor::contract::ContractConfig;
use crate::executor::dispatch::{
    FallbackBackend, FallbackTargetConfig, ResolvedBackend, ResolvedHostPoolConfig,
    ResolvedHostPoolMember, ResolvedTarget, ResolvedValidation,
};
use crate::executor::local::{LocalTargetConfig, LocalValidationConfig};
use crate::executor::ssh::{SshTargetConfig, SshValidation};
use crate::executor::ssh_windows::{WindowsTargetConfig, WindowsValidation};
use crate::job::{Priority, ValidationMode};
use crate::ship::{RunExecutionRequest, ShipExecutionRequest};
use crate::ship_state::ShipState;
use crate::warm_pool::{is_backend_eligible, warm_host_key};

/// Current queued-execution schema.
pub const QUEUED_EXECUTION_SCHEMA_VERSION: u32 = 1;

/// Fallible queued request/outcome store operation result.
pub type QueueRequestResult<T> = Result<T, QueueRequestError>;

/// Durable queued request/outcome store error.
#[derive(Debug)]
pub enum QueueRequestError {
    /// Filesystem operation failed.
    Io(io::Error),
    /// JSON serialization or parsing failed.
    Json(serde_json::Error),
    /// The on-disk schema is newer or otherwise unsupported.
    UnsupportedSchema {
        /// Observed schema version.
        version: u32,
    },
    /// Durable request snapshot cannot be converted back to executable inputs.
    InvalidSnapshot {
        /// Human-readable reason.
        reason: String,
    },
}

impl Display for QueueRequestError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "queue request I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "queue request JSON failed: {error}"),
            Self::UnsupportedSchema { version } => {
                write!(
                    formatter,
                    "unsupported queue request schema version {version}"
                )
            }
            Self::InvalidSnapshot { reason } => {
                write!(formatter, "invalid queue request snapshot: {reason}")
            }
        }
    }
}

impl std::error::Error for QueueRequestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::UnsupportedSchema { .. } | Self::InvalidSnapshot { .. } => None,
        }
    }
}

impl From<io::Error> for QueueRequestError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for QueueRequestError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// JSON-backed queued execution request store.
#[derive(Clone, Debug)]
pub struct QueueRequestStore {
    path: PathBuf,
}

impl QueueRequestStore {
    /// Open a request store rooted at `<state_dir>/queue/requests`.
    pub fn new(state_dir: impl Into<PathBuf>) -> io::Result<Self> {
        let path = state_dir.into().join("queue").join("requests");
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Store directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Request path for a job id.
    #[must_use]
    pub fn path_for(&self, job_id: &str) -> PathBuf {
        self.path.join(format!("{job_id}.json"))
    }

    /// Save one request envelope atomically.
    pub fn save(&self, envelope: &QueuedExecutionEnvelope) -> QueueRequestResult<()> {
        if envelope.schema_version != QUEUED_EXECUTION_SCHEMA_VERSION {
            return Err(QueueRequestError::UnsupportedSchema {
                version: envelope.schema_version,
            });
        }
        write_json_atomic(&self.path_for(&envelope.job_id), envelope)
    }

    /// Load one request envelope.
    pub fn load(&self, job_id: &str) -> QueueRequestResult<Option<QueuedExecutionEnvelope>> {
        read_versioned_json(&self.path_for(job_id))
    }

    /// Delete one request envelope, if present.
    pub fn delete(&self, job_id: &str) -> QueueRequestResult<bool> {
        delete_if_present(&self.path_for(job_id))
    }

    /// Delete request envelopes whose job id is no longer present in the queue
    /// and whose file age is beyond `grace`.
    pub fn sweep_absent_older_than(
        &self,
        active_job_ids: &BTreeSet<String>,
        grace: Duration,
    ) -> QueueRequestResult<Vec<String>> {
        sweep_absent_older_than(&self.path, active_job_ids, grace)
    }
}

/// JSON-backed queued execution outcome store.
#[derive(Clone, Debug)]
pub struct QueueOutcomeStore {
    path: PathBuf,
}

impl QueueOutcomeStore {
    /// Open an outcome store rooted at `<state_dir>/queue/outcomes`.
    pub fn new(state_dir: impl Into<PathBuf>) -> io::Result<Self> {
        let path = state_dir.into().join("queue").join("outcomes");
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Store directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Outcome path for a job id.
    #[must_use]
    pub fn path_for(&self, job_id: &str) -> PathBuf {
        self.path.join(format!("{job_id}.json"))
    }

    /// Save one outcome envelope atomically.
    pub fn save(&self, outcome: &QueuedExecutionOutcome) -> QueueRequestResult<()> {
        if outcome.schema_version() != QUEUED_EXECUTION_SCHEMA_VERSION {
            return Err(QueueRequestError::UnsupportedSchema {
                version: outcome.schema_version(),
            });
        }
        write_json_atomic(&self.path_for(outcome.job_id()), outcome)
    }

    /// Load one outcome envelope.
    pub fn load(&self, job_id: &str) -> QueueRequestResult<Option<QueuedExecutionOutcome>> {
        read_versioned_json(&self.path_for(job_id))
    }

    /// Delete one outcome envelope, if present.
    pub fn delete(&self, job_id: &str) -> QueueRequestResult<bool> {
        delete_if_present(&self.path_for(job_id))
    }

    /// Delete outcome envelopes whose job id is no longer present in the queue
    /// and whose file age is beyond `grace`.
    pub fn sweep_absent_older_than(
        &self,
        active_job_ids: &BTreeSet<String>,
        grace: Duration,
    ) -> QueueRequestResult<Vec<String>> {
        sweep_absent_older_than(&self.path, active_job_ids, grace)
    }
}

/// Durable queued execution request envelope.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedExecutionEnvelope {
    /// Schema version.
    pub schema_version: u32,
    /// Job identifier.
    pub job_id: String,
    /// Execution kind.
    pub kind: QueuedExecutionKind,
    /// Original CLI working directory.
    pub cwd: PathBuf,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Scheduler-facing resource plan snapshot.
    pub resource_plan: JobResourcePlan,
    /// Resolved execution request.
    pub request: QueuedExecutionRequest,
}

impl QueuedExecutionEnvelope {
    /// Build a queued `run` request envelope.
    #[must_use]
    pub fn from_run_request(
        job_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        request: &RunExecutionRequest,
    ) -> Self {
        let job_id = job_id.into();
        let cwd = cwd.into();
        Self {
            schema_version: QUEUED_EXECUTION_SCHEMA_VERSION,
            job_id,
            kind: QueuedExecutionKind::Run,
            cwd: cwd.clone(),
            created_at: Utc::now(),
            resource_plan: JobResourcePlan::from_run_request(&cwd, request),
            request: QueuedExecutionRequest::Run(QueuedRunRequest::from(request)),
        }
    }

    /// Build a queued `ship` request envelope.
    #[must_use]
    pub fn from_ship_request(
        job_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        request: &ShipExecutionRequest,
    ) -> Self {
        let job_id = job_id.into();
        let cwd = cwd.into();
        Self {
            schema_version: QUEUED_EXECUTION_SCHEMA_VERSION,
            job_id,
            kind: QueuedExecutionKind::Ship,
            cwd: cwd.clone(),
            created_at: Utc::now(),
            resource_plan: JobResourcePlan::from_ship_request(&cwd, request),
            request: QueuedExecutionRequest::Ship(QueuedShipRequest::from(request)),
        }
    }

    /// Convert this durable request envelope back into an executable run
    /// request.
    pub fn to_run_request(&self) -> QueueRequestResult<RunExecutionRequest> {
        let QueuedExecutionRequest::Run(request) = &self.request else {
            return Err(invalid_snapshot("queued request is not a run request"));
        };
        request.to_execution_request()
    }

    /// Convert this durable request envelope back into an executable ship
    /// request.
    pub fn to_ship_request(&self) -> QueueRequestResult<ShipExecutionRequest> {
        let QueuedExecutionRequest::Ship(request) = &self.request else {
            return Err(invalid_snapshot("queued request is not a ship request"));
        };
        request.to_execution_request()
    }
}

/// Queued execution kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QueuedExecutionKind {
    /// `shipyard run`.
    Run,
    /// `shipyard ship`.
    Ship,
}

/// Queued execution request payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum QueuedExecutionRequest {
    /// `shipyard run` request.
    Run(QueuedRunRequest),
    /// `shipyard ship` request.
    Ship(QueuedShipRequest),
}

/// Durable `shipyard run` request snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedRunRequest {
    /// Branch under validation.
    pub branch: String,
    /// Head SHA.
    pub sha: String,
    /// Validation mode.
    pub mode: ValidationMode,
    /// Scheduling priority.
    pub priority: Priority,
    /// Whether warm-pool reuse is disabled for this run.
    pub warm_disabled: bool,
    /// Whether remaining targets should be skipped after the first failure.
    pub fail_fast: bool,
    /// Optional explicit resume stage.
    pub resume_from: Option<String>,
    /// Ordered resolved target snapshots.
    pub targets: Vec<QueuedResolvedTarget>,
}

impl From<&RunExecutionRequest> for QueuedRunRequest {
    fn from(request: &RunExecutionRequest) -> Self {
        Self {
            branch: request.branch.clone(),
            sha: request.sha.clone(),
            mode: request.mode,
            priority: request.priority,
            warm_disabled: request.warm_disabled,
            fail_fast: request.fail_fast,
            resume_from: request.resume_from.clone(),
            targets: snapshot_targets(&request.targets),
        }
    }
}

impl QueuedRunRequest {
    /// Convert this durable request snapshot back into executable inputs.
    pub fn to_execution_request(&self) -> QueueRequestResult<RunExecutionRequest> {
        Ok(RunExecutionRequest {
            branch: self.branch.clone(),
            sha: self.sha.clone(),
            mode: self.mode,
            priority: self.priority,
            warm_disabled: self.warm_disabled,
            fail_fast: self.fail_fast,
            resume_from: self.resume_from.clone(),
            targets: restore_targets(&self.targets)?,
        })
    }
}

/// Durable `shipyard ship` request snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedShipRequest {
    /// Pull request number.
    pub pr: u64,
    /// Repository slug.
    pub repo: String,
    /// Head branch.
    pub branch: String,
    /// Base branch.
    pub base_branch: String,
    /// Head SHA.
    pub sha: String,
    /// Optional commit subject.
    pub commit_subject: String,
    /// Optional PR URL resolved from GitHub.
    pub pr_url: Option<String>,
    /// Optional PR title resolved from GitHub.
    pub pr_title: Option<String>,
    /// Validation mode.
    pub mode: ValidationMode,
    /// Queue priority.
    pub priority: Priority,
    /// Whether warm-pool reuse is disabled for this run.
    pub warm_disabled: bool,
    /// Whether remaining targets should be skipped after the first failure.
    pub fail_fast: bool,
    /// Optional explicit resume stage.
    pub resume_from: Option<String>,
    /// Target names whose failures should not block merge.
    pub advisory_targets: BTreeSet<String>,
    /// Ordered resolved target snapshots.
    pub targets: Vec<QueuedResolvedTarget>,
}

impl From<&ShipExecutionRequest> for QueuedShipRequest {
    fn from(request: &ShipExecutionRequest) -> Self {
        Self {
            pr: request.pr,
            repo: request.repo.clone(),
            branch: request.branch.clone(),
            base_branch: request.base_branch.clone(),
            sha: request.sha.clone(),
            commit_subject: request.commit_subject.clone(),
            pr_url: request.pr_url.clone(),
            pr_title: request.pr_title.clone(),
            mode: request.mode,
            priority: request.priority,
            warm_disabled: request.warm_disabled,
            fail_fast: request.fail_fast,
            resume_from: request.resume_from.clone(),
            advisory_targets: request.advisory_targets.clone(),
            targets: snapshot_targets(&request.targets),
        }
    }
}

impl QueuedShipRequest {
    /// Convert this durable request snapshot back into executable inputs.
    pub fn to_execution_request(&self) -> QueueRequestResult<ShipExecutionRequest> {
        Ok(ShipExecutionRequest {
            pr: self.pr,
            repo: self.repo.clone(),
            branch: self.branch.clone(),
            base_branch: self.base_branch.clone(),
            sha: self.sha.clone(),
            commit_subject: self.commit_subject.clone(),
            pr_url: self.pr_url.clone(),
            pr_title: self.pr_title.clone(),
            mode: self.mode,
            priority: self.priority,
            warm_disabled: self.warm_disabled,
            fail_fast: self.fail_fast,
            resume_from: self.resume_from.clone(),
            advisory_targets: self.advisory_targets.clone(),
            targets: restore_targets(&self.targets)?,
        })
    }
}

/// Scheduler-facing resource plan snapshot.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobResourcePlan {
    /// Logical targets in execution order.
    pub targets: Vec<String>,
    /// Scheduler-exclusive resource claims. Jobs with intersecting claims must
    /// not run concurrently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusive_claims: Vec<String>,
    /// Cloud targets that consume cloud runner capacity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cloud_targets: Vec<String>,
    /// Host-pool lease demands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_pools: Vec<HostPoolDemand>,
}

impl JobResourcePlan {
    /// Build a resource plan from resolved targets.
    #[must_use]
    pub fn from_targets(targets: &[ResolvedTarget]) -> Self {
        Self::from_targets_with_context("", Path::new("."), None, targets)
    }

    /// Build a resource plan for a run request.
    #[must_use]
    pub fn from_run_request(cwd: &Path, request: &RunExecutionRequest) -> Self {
        Self::from_targets_with_context(&request.branch, cwd, None, &request.targets)
    }

    /// Build a resource plan for a ship request.
    #[must_use]
    pub fn from_ship_request(cwd: &Path, request: &ShipExecutionRequest) -> Self {
        Self::from_targets_with_context(
            &request.branch,
            cwd,
            Some((&request.repo, request.pr)),
            &request.targets,
        )
    }

    fn from_targets_with_context(
        branch: &str,
        cwd: &Path,
        ship_scope: Option<(&str, u64)>,
        targets: &[ResolvedTarget],
    ) -> Self {
        let mut plan = Self {
            targets: targets.iter().map(|target| target.name.clone()).collect(),
            exclusive_claims: Vec::new(),
            cloud_targets: Vec::new(),
            host_pools: Vec::new(),
        };
        if let Some((repo, pr)) = ship_scope {
            plan.exclusive_claims
                .push(format!("ship-state:{repo}:pr-{pr}"));
        }
        for target in targets {
            if !branch.is_empty() {
                plan.exclusive_claims
                    .push(format!("evidence:{branch}:{}", target.name));
            }
            if target.warm_keepalive_seconds > 0 && is_backend_eligible(&target.backend_name) {
                plan.exclusive_claims.push(format!(
                    "warm:{}:{}",
                    target.name,
                    warm_host_key(target.host.as_deref())
                ));
            }
            collect_backend_resource_demands(target, &mut plan, cwd);
        }
        plan.exclusive_claims.sort();
        plan.exclusive_claims.dedup();
        plan.cloud_targets.sort();
        plan.cloud_targets.dedup();
        plan.host_pools
            .sort_by(|left, right| left.pool_name.cmp(&right.pool_name));
        plan
    }
}

/// One host-pool lease demand.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct HostPoolDemand {
    /// Pool name.
    pub pool_name: String,
    /// Capabilities required by the logical target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Number of pool lease slots required by this logical target.
    #[serde(default = "default_host_pool_slots")]
    pub slots: u32,
    /// Stable capability key used by scheduler capacity accounting.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub capability_key: String,
}

/// Durable resolved target snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedResolvedTarget {
    /// Logical target name.
    pub name: String,
    /// Platform label.
    pub platform: String,
    /// Normalized backend label.
    pub backend_name: String,
    /// Configured warm-pool keepalive in seconds.
    pub warm_keepalive_seconds: u32,
    /// Host key input, when this target has a remote host.
    pub host: Option<String>,
    /// Backend-specific target snapshot.
    pub backend: QueuedBackendSnapshot,
    /// Backend-specific validation snapshot.
    pub validation: QueuedValidationSnapshot,
    /// Optional failure parser selection.
    pub failure_parser: Option<String>,
}

impl From<&ResolvedTarget> for QueuedResolvedTarget {
    fn from(target: &ResolvedTarget) -> Self {
        Self {
            name: target.name.clone(),
            platform: target.platform.clone(),
            backend_name: target.backend_name.clone(),
            warm_keepalive_seconds: target.warm_keepalive_seconds,
            host: target.host.clone(),
            backend: QueuedBackendSnapshot::from(&target.backend),
            validation: QueuedValidationSnapshot::from(&target.validation),
            failure_parser: target.failure_parser.clone(),
        }
    }
}

/// Backend-specific durable snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum QueuedBackendSnapshot {
    /// Local process execution.
    Local(QueuedLocalTarget),
    /// POSIX SSH execution.
    Ssh(QueuedSshTarget),
    /// Windows SSH execution.
    Windows(QueuedWindowsTarget),
    /// GitHub Actions cloud execution.
    Cloud(QueuedCloudTarget),
    /// Ordered local host-pool execution.
    HostPool(QueuedHostPoolTarget),
    /// Ordered fallback chain.
    Fallback(QueuedFallbackTarget),
}

impl From<&ResolvedBackend> for QueuedBackendSnapshot {
    fn from(backend: &ResolvedBackend) -> Self {
        match backend {
            ResolvedBackend::Local(target) => Self::Local(QueuedLocalTarget::from(target)),
            ResolvedBackend::Ssh(target) => Self::Ssh(QueuedSshTarget::from(target)),
            ResolvedBackend::Windows(target) => Self::Windows(QueuedWindowsTarget::from(target)),
            ResolvedBackend::Cloud(target) => Self::Cloud(QueuedCloudTarget::from(target)),
            ResolvedBackend::HostPool(target) => Self::HostPool(QueuedHostPoolTarget::from(target)),
            ResolvedBackend::Fallback(target) => Self::Fallback(QueuedFallbackTarget::from(target)),
        }
    }
}

/// Local target snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedLocalTarget {
    /// Target name.
    pub name: String,
    /// Platform label.
    pub platform: String,
    /// Optional working directory.
    pub cwd: Option<PathBuf>,
    /// Wall-clock timeout in seconds.
    pub timeout_secs: u64,
}

impl From<&LocalTargetConfig> for QueuedLocalTarget {
    fn from(target: &LocalTargetConfig) -> Self {
        Self {
            name: target.name.clone(),
            platform: target.platform.clone(),
            cwd: target.cwd.clone(),
            timeout_secs: target.timeout_secs,
        }
    }
}

/// POSIX SSH target snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedSshTarget {
    /// Target name.
    pub name: String,
    /// Platform label.
    pub platform: String,
    /// SSH host, including optional user.
    pub host: Option<String>,
    /// Remote git checkout path.
    pub repo_path: String,
    /// Configured SSH options.
    pub ssh_options: Vec<String>,
    /// Optional identity file appended to SSH options.
    pub identity_file: Option<String>,
    /// Remote path used for bundle upload.
    pub remote_bundle_path: String,
    /// Optional local repository directory used for bundle creation.
    pub local_repo_dir: Option<PathBuf>,
    /// Validation timeout in seconds.
    pub timeout_secs: u64,
    /// Bundle upload timeout in seconds.
    pub bundle_upload_timeout_secs: u64,
    /// Bundle apply timeout in seconds.
    pub bundle_apply_timeout_secs: u64,
}

impl From<&SshTargetConfig> for QueuedSshTarget {
    fn from(target: &SshTargetConfig) -> Self {
        Self {
            name: target.name.clone(),
            platform: target.platform.clone(),
            host: target.host.clone(),
            repo_path: target.repo_path.clone(),
            ssh_options: target.ssh_options.clone(),
            identity_file: target.identity_file.clone(),
            remote_bundle_path: target.remote_bundle_path.clone(),
            local_repo_dir: target.local_repo_dir.clone(),
            timeout_secs: target.timeout_secs,
            bundle_upload_timeout_secs: target.bundle_upload_timeout_secs,
            bundle_apply_timeout_secs: target.bundle_apply_timeout_secs,
        }
    }
}

/// Windows SSH target snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedWindowsTarget {
    /// Common POSIX SSH fields.
    pub ssh: QueuedSshTarget,
    /// Whether to run Visual Studio toolchain detection.
    pub windows_vs_detect: bool,
    /// Whether to serialize validation with a host-wide mutex.
    pub windows_host_mutex: bool,
    /// Optional host mutex name.
    pub windows_host_mutex_name: String,
}

impl From<&WindowsTargetConfig> for QueuedWindowsTarget {
    fn from(target: &WindowsTargetConfig) -> Self {
        Self {
            ssh: QueuedSshTarget {
                name: target.name.clone(),
                platform: target.platform.clone(),
                host: target.host.clone(),
                repo_path: target.repo_path.clone(),
                ssh_options: target.ssh_options.clone(),
                identity_file: target.identity_file.clone(),
                remote_bundle_path: target.remote_bundle_path.clone(),
                local_repo_dir: target.local_repo_dir.clone(),
                timeout_secs: target.timeout_secs,
                bundle_upload_timeout_secs: target.bundle_upload_timeout_secs,
                bundle_apply_timeout_secs: target.bundle_apply_timeout_secs,
            },
            windows_vs_detect: target.windows_vs_detect,
            windows_host_mutex: target.windows_host_mutex,
            windows_host_mutex_name: target.windows_host_mutex_name.clone(),
        }
    }
}

/// GitHub Actions cloud target snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedCloudTarget {
    /// Target name.
    pub name: String,
    /// Platform label.
    pub platform: String,
    /// Workflow file or name.
    pub workflow: String,
    /// Optional repository slug.
    pub repository: Option<String>,
    /// Runner provider.
    pub runner_provider: Option<String>,
    /// Optional runner selector/profile.
    pub runner_selector: Option<String>,
    /// Optional runner override map.
    pub runner_overrides: BTreeMap<String, String>,
    /// Poll cadence while waiting for the run to appear and finish.
    pub poll_interval_secs: u64,
    /// Maximum wait for the dispatched run to appear.
    pub dispatch_settle_secs: u64,
    /// Maximum wait for the workflow run to complete.
    pub max_poll_secs: u64,
    /// Optional per-target failure parser name.
    pub failure_parser: Option<String>,
}

impl From<&CloudTargetConfig> for QueuedCloudTarget {
    fn from(target: &CloudTargetConfig) -> Self {
        Self {
            name: target.name.clone(),
            platform: target.platform.clone(),
            workflow: target.workflow.clone(),
            repository: target.repository.clone(),
            runner_provider: target.runner_provider.clone(),
            runner_selector: target.runner_selector.clone(),
            runner_overrides: target.runner_overrides.clone(),
            poll_interval_secs: target.poll_interval_secs,
            dispatch_settle_secs: target.dispatch_settle_secs,
            max_poll_secs: target.max_poll_secs,
            failure_parser: target.failure_parser.clone(),
        }
    }
}

/// Host-pool target snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedHostPoolTarget {
    /// Pool name.
    pub pool_name: String,
    /// Member selection strategy.
    pub strategy: String,
    /// Seconds after which an unrefreshed lease is stale.
    pub lease_stale_seconds: u64,
    /// Seconds between lease heartbeat refreshes.
    pub heartbeat_interval_seconds: u64,
    /// Capabilities required by the target.
    pub requires: Vec<String>,
    /// Resolved concrete member targets in selection order.
    pub members: Vec<QueuedHostPoolMember>,
}

impl From<&ResolvedHostPoolConfig> for QueuedHostPoolTarget {
    fn from(target: &ResolvedHostPoolConfig) -> Self {
        Self {
            pool_name: target.pool_name.clone(),
            strategy: target.strategy.clone(),
            lease_stale_seconds: target.lease_stale_seconds,
            heartbeat_interval_seconds: target.heartbeat_interval_seconds,
            requires: target.requires.clone(),
            members: target
                .members
                .iter()
                .map(QueuedHostPoolMember::from)
                .collect(),
        }
    }
}

/// Host-pool member snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedHostPoolMember {
    /// Stable member id.
    pub id: String,
    /// Concrete target used by the existing local or SSH executor.
    pub target: Box<QueuedResolvedTarget>,
    /// User-facing member label.
    pub label: String,
    /// User-facing profile label for capability mismatch errors.
    pub profile_label: String,
    /// Max concurrent leases for this member.
    pub max_concurrency: u32,
    /// Member capabilities.
    pub capabilities: Vec<String>,
}

impl From<&ResolvedHostPoolMember> for QueuedHostPoolMember {
    fn from(member: &ResolvedHostPoolMember) -> Self {
        Self {
            id: member.id.clone(),
            target: Box::new(QueuedResolvedTarget::from(member.target.as_ref())),
            label: member.label.clone(),
            profile_label: member.profile_label.clone(),
            max_concurrency: member.max_concurrency,
            capabilities: member.capabilities.clone(),
        }
    }
}

/// Fallback chain snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedFallbackTarget {
    /// Backends in attempt order.
    pub backends: Vec<QueuedFallbackBackend>,
    /// Required capabilities for locality routing.
    pub requires: Vec<String>,
    /// Heartbeat age that demotes non-passing stale results.
    pub heartbeat_stale_secs: u64,
}

impl From<&FallbackTargetConfig> for QueuedFallbackTarget {
    fn from(target: &FallbackTargetConfig) -> Self {
        Self {
            backends: target
                .backends
                .iter()
                .map(QueuedFallbackBackend::from)
                .collect(),
            requires: target.requires.clone(),
            heartbeat_stale_secs: target.heartbeat_stale_secs,
        }
    }
}

/// Fallback backend snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedFallbackBackend {
    /// Fully resolved backend target.
    pub target: Box<QueuedResolvedTarget>,
    /// User-facing backend label.
    pub label: String,
    /// User-facing profile label for capability mismatch errors.
    pub profile_label: String,
    /// Inline backend capabilities.
    pub capabilities: Vec<String>,
}

impl From<&FallbackBackend> for QueuedFallbackBackend {
    fn from(backend: &FallbackBackend) -> Self {
        Self {
            target: Box::new(QueuedResolvedTarget::from(backend.target.as_ref())),
            label: backend.label.clone(),
            profile_label: backend.profile_label.clone(),
            capabilities: backend.capabilities.clone(),
        }
    }
}

/// Validation-specific durable snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum QueuedValidationSnapshot {
    /// Local validation settings.
    Local(QueuedLocalValidation),
    /// POSIX SSH validation settings.
    Ssh {
        /// Command/stage shape.
        validation: QueuedRemoteValidation,
        /// Optional validation contract.
        contract: Option<QueuedContract>,
    },
    /// Windows SSH validation settings.
    Windows {
        /// Command/stage shape.
        validation: QueuedRemoteValidation,
        /// Optional validation contract.
        contract: Option<QueuedContract>,
    },
    /// Cloud validation settings.
    Cloud,
    /// Host-pool validation settings.
    HostPool,
    /// Fallback validation settings.
    Fallback,
}

impl From<&ResolvedValidation> for QueuedValidationSnapshot {
    fn from(validation: &ResolvedValidation) -> Self {
        match validation {
            ResolvedValidation::Local(validation) => {
                Self::Local(QueuedLocalValidation::from(validation))
            }
            ResolvedValidation::Ssh {
                validation,
                contract,
            } => Self::Ssh {
                validation: QueuedRemoteValidation::from(validation),
                contract: contract.as_ref().map(QueuedContract::from),
            },
            ResolvedValidation::Windows {
                validation,
                contract,
            } => Self::Windows {
                validation: QueuedRemoteValidation::from(validation),
                contract: contract.as_ref().map(QueuedContract::from),
            },
            ResolvedValidation::Cloud => Self::Cloud,
            ResolvedValidation::HostPool => Self::HostPool,
            ResolvedValidation::Fallback => Self::Fallback,
        }
    }
}

/// Local validation snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedLocalValidation {
    /// Optional single command.
    pub command: Option<String>,
    /// Stage commands keyed by stage name.
    pub stages: BTreeMap<String, String>,
    /// Optional validation contract.
    pub contract: Option<QueuedContract>,
    /// Whether prepared-state stage skipping is enabled.
    pub prepared_state_enabled: bool,
    /// Suppress staged working-tree drift detection.
    pub allow_tree_drift: bool,
}

impl From<&LocalValidationConfig> for QueuedLocalValidation {
    fn from(validation: &LocalValidationConfig) -> Self {
        Self {
            command: validation.command.clone(),
            stages: validation.stages.clone(),
            contract: validation.contract.as_ref().map(QueuedContract::from),
            prepared_state_enabled: validation.prepared_state_enabled,
            allow_tree_drift: validation.allow_tree_drift,
        }
    }
}

/// Remote command or staged validation snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum QueuedRemoteValidation {
    /// A single validation command string.
    Command {
        /// Shell command to execute.
        command: String,
    },
    /// Ordered validation stages keyed by Shipyard's stage names.
    Stages {
        /// Stage commands keyed by stage name.
        stages: BTreeMap<String, String>,
    },
}

impl From<&SshValidation> for QueuedRemoteValidation {
    fn from(validation: &SshValidation) -> Self {
        match validation {
            SshValidation::Command(command) => Self::Command {
                command: command.clone(),
            },
            SshValidation::Stages(stages) => Self::Stages {
                stages: stages.clone(),
            },
        }
    }
}

impl From<&WindowsValidation> for QueuedRemoteValidation {
    fn from(validation: &WindowsValidation) -> Self {
        match validation {
            WindowsValidation::Command(command) => Self::Command {
                command: command.clone(),
            },
            WindowsValidation::Stages(stages) => Self::Stages {
                stages: stages.clone(),
            },
        }
    }
}

/// Validation contract snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedContract {
    /// Declared output markers.
    pub markers: Vec<String>,
    /// Whether at least one marker is enough.
    pub require_at_least_one: bool,
    /// Whether violation should force the result to fail.
    pub enforce: bool,
}

impl From<&ContractConfig> for QueuedContract {
    fn from(contract: &ContractConfig) -> Self {
        Self {
            markers: contract.markers.clone(),
            require_at_least_one: contract.require_at_least_one,
            enforce: contract.enforce,
        }
    }
}

/// Durable queued execution outcome envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
#[allow(clippy::large_enum_variant)]
pub enum QueuedExecutionOutcome {
    /// `shipyard run` outcome.
    Run {
        /// Schema version.
        schema_version: u32,
        /// Job id.
        job_id: String,
    },
    /// `shipyard ship` outcome.
    Ship {
        /// Schema version.
        schema_version: u32,
        /// Job id.
        job_id: String,
        /// Pull request number.
        pr: u64,
        /// Final ship state.
        ship_state: ShipState,
        /// Whether an existing compatible state was reused.
        resumed_existing_state: bool,
    },
}

impl QueuedExecutionOutcome {
    /// Build a run outcome.
    #[must_use]
    pub fn run(job_id: impl Into<String>) -> Self {
        Self::Run {
            schema_version: QUEUED_EXECUTION_SCHEMA_VERSION,
            job_id: job_id.into(),
        }
    }

    /// Build a ship outcome.
    #[must_use]
    pub fn ship(
        job_id: impl Into<String>,
        pr: u64,
        ship_state: ShipState,
        resumed_existing_state: bool,
    ) -> Self {
        Self::Ship {
            schema_version: QUEUED_EXECUTION_SCHEMA_VERSION,
            job_id: job_id.into(),
            pr,
            ship_state,
            resumed_existing_state,
        }
    }

    /// Return this outcome's schema version.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        match self {
            Self::Run { schema_version, .. } | Self::Ship { schema_version, .. } => *schema_version,
        }
    }

    /// Return this outcome's job id.
    #[must_use]
    pub fn job_id(&self) -> &str {
        match self {
            Self::Run { job_id, .. } | Self::Ship { job_id, .. } => job_id,
        }
    }
}

fn snapshot_targets(targets: &[ResolvedTarget]) -> Vec<QueuedResolvedTarget> {
    targets.iter().map(QueuedResolvedTarget::from).collect()
}

fn restore_targets(targets: &[QueuedResolvedTarget]) -> QueueRequestResult<Vec<ResolvedTarget>> {
    targets.iter().map(restore_target).collect()
}

fn restore_target(target: &QueuedResolvedTarget) -> QueueRequestResult<ResolvedTarget> {
    Ok(ResolvedTarget {
        name: target.name.clone(),
        platform: target.platform.clone(),
        backend_name: target.backend_name.clone(),
        warm_keepalive_seconds: target.warm_keepalive_seconds,
        host: target.host.clone(),
        backend: restore_backend(&target.backend)?,
        validation: restore_validation(&target.validation),
        failure_parser: target.failure_parser.clone(),
    })
}

fn restore_backend(backend: &QueuedBackendSnapshot) -> QueueRequestResult<ResolvedBackend> {
    Ok(match backend {
        QueuedBackendSnapshot::Local(target) => ResolvedBackend::Local(LocalTargetConfig {
            name: target.name.clone(),
            platform: target.platform.clone(),
            cwd: target.cwd.clone(),
            timeout_secs: target.timeout_secs,
        }),
        QueuedBackendSnapshot::Ssh(target) => ResolvedBackend::Ssh(restore_ssh_target(target)),
        QueuedBackendSnapshot::Windows(target) => ResolvedBackend::Windows(WindowsTargetConfig {
            name: target.ssh.name.clone(),
            platform: target.ssh.platform.clone(),
            host: target.ssh.host.clone(),
            repo_path: target.ssh.repo_path.clone(),
            ssh_options: target.ssh.ssh_options.clone(),
            identity_file: target.ssh.identity_file.clone(),
            remote_bundle_path: target.ssh.remote_bundle_path.clone(),
            local_repo_dir: target.ssh.local_repo_dir.clone(),
            timeout_secs: target.ssh.timeout_secs,
            bundle_upload_timeout_secs: target.ssh.bundle_upload_timeout_secs,
            bundle_apply_timeout_secs: target.ssh.bundle_apply_timeout_secs,
            windows_vs_detect: target.windows_vs_detect,
            windows_host_mutex: target.windows_host_mutex,
            windows_host_mutex_name: target.windows_host_mutex_name.clone(),
        }),
        QueuedBackendSnapshot::Cloud(target) => ResolvedBackend::Cloud(CloudTargetConfig {
            name: target.name.clone(),
            platform: target.platform.clone(),
            workflow: target.workflow.clone(),
            repository: target.repository.clone(),
            runner_provider: target.runner_provider.clone(),
            runner_selector: target.runner_selector.clone(),
            runner_overrides: target.runner_overrides.clone(),
            poll_interval_secs: target.poll_interval_secs,
            dispatch_settle_secs: target.dispatch_settle_secs,
            max_poll_secs: target.max_poll_secs,
            failure_parser: target.failure_parser.clone(),
        }),
        QueuedBackendSnapshot::HostPool(target) => {
            ResolvedBackend::HostPool(ResolvedHostPoolConfig {
                pool_name: target.pool_name.clone(),
                strategy: target.strategy.clone(),
                lease_stale_seconds: target.lease_stale_seconds,
                heartbeat_interval_seconds: target.heartbeat_interval_seconds,
                requires: target.requires.clone(),
                members: target
                    .members
                    .iter()
                    .map(restore_host_pool_member)
                    .collect::<QueueRequestResult<Vec<_>>>()?,
            })
        }
        QueuedBackendSnapshot::Fallback(target) => {
            ResolvedBackend::Fallback(FallbackTargetConfig {
                backends: target
                    .backends
                    .iter()
                    .map(restore_fallback_backend)
                    .collect::<QueueRequestResult<Vec<_>>>()?,
                requires: target.requires.clone(),
                heartbeat_stale_secs: target.heartbeat_stale_secs,
            })
        }
    })
}

fn restore_ssh_target(target: &QueuedSshTarget) -> SshTargetConfig {
    SshTargetConfig {
        name: target.name.clone(),
        platform: target.platform.clone(),
        host: target.host.clone(),
        repo_path: target.repo_path.clone(),
        ssh_options: target.ssh_options.clone(),
        identity_file: target.identity_file.clone(),
        remote_bundle_path: target.remote_bundle_path.clone(),
        local_repo_dir: target.local_repo_dir.clone(),
        timeout_secs: target.timeout_secs,
        bundle_upload_timeout_secs: target.bundle_upload_timeout_secs,
        bundle_apply_timeout_secs: target.bundle_apply_timeout_secs,
    }
}

fn restore_host_pool_member(
    member: &QueuedHostPoolMember,
) -> QueueRequestResult<ResolvedHostPoolMember> {
    Ok(ResolvedHostPoolMember {
        id: member.id.clone(),
        target: Box::new(restore_target(&member.target)?),
        label: member.label.clone(),
        profile_label: member.profile_label.clone(),
        max_concurrency: member.max_concurrency,
        capabilities: member.capabilities.clone(),
    })
}

fn restore_fallback_backend(
    backend: &QueuedFallbackBackend,
) -> QueueRequestResult<FallbackBackend> {
    Ok(FallbackBackend {
        target: Box::new(restore_target(&backend.target)?),
        label: backend.label.clone(),
        profile_label: backend.profile_label.clone(),
        capabilities: backend.capabilities.clone(),
    })
}

fn restore_validation(validation: &QueuedValidationSnapshot) -> ResolvedValidation {
    match validation {
        QueuedValidationSnapshot::Local(validation) => {
            ResolvedValidation::Local(LocalValidationConfig {
                command: validation.command.clone(),
                stages: validation.stages.clone(),
                contract: validation.contract.as_ref().map(restore_contract),
                prepared_state_enabled: validation.prepared_state_enabled,
                allow_tree_drift: validation.allow_tree_drift,
            })
        }
        QueuedValidationSnapshot::Ssh {
            validation,
            contract,
        } => ResolvedValidation::Ssh {
            validation: restore_ssh_validation(validation),
            contract: contract.as_ref().map(restore_contract),
        },
        QueuedValidationSnapshot::Windows {
            validation,
            contract,
        } => ResolvedValidation::Windows {
            validation: restore_windows_validation(validation),
            contract: contract.as_ref().map(restore_contract),
        },
        QueuedValidationSnapshot::Cloud => ResolvedValidation::Cloud,
        QueuedValidationSnapshot::HostPool => ResolvedValidation::HostPool,
        QueuedValidationSnapshot::Fallback => ResolvedValidation::Fallback,
    }
}

fn restore_ssh_validation(validation: &QueuedRemoteValidation) -> SshValidation {
    match validation {
        QueuedRemoteValidation::Command { command } => SshValidation::Command(command.clone()),
        QueuedRemoteValidation::Stages { stages } => SshValidation::Stages(stages.clone()),
    }
}

fn restore_windows_validation(validation: &QueuedRemoteValidation) -> WindowsValidation {
    match validation {
        QueuedRemoteValidation::Command { command } => WindowsValidation::Command(command.clone()),
        QueuedRemoteValidation::Stages { stages } => WindowsValidation::Stages(stages.clone()),
    }
}

fn restore_contract(contract: &QueuedContract) -> ContractConfig {
    ContractConfig {
        markers: contract.markers.clone(),
        require_at_least_one: contract.require_at_least_one,
        enforce: contract.enforce,
    }
}

fn invalid_snapshot(reason: impl Into<String>) -> QueueRequestError {
    QueueRequestError::InvalidSnapshot {
        reason: reason.into(),
    }
}

fn collect_backend_resource_demands(
    target: &ResolvedTarget,
    plan: &mut JobResourcePlan,
    cwd: &Path,
) {
    match &target.backend {
        ResolvedBackend::Local(local) => {
            let target_cwd = local.cwd.as_deref().unwrap_or(cwd);
            plan.exclusive_claims
                .push(format!("local-cwd:{}", normalized_path_claim(target_cwd)));
        }
        ResolvedBackend::Ssh(ssh) => {
            plan.exclusive_claims.push(format!(
                "ssh-repo:{}:{}",
                ssh.host.as_deref().unwrap_or("?"),
                ssh.repo_path
            ));
        }
        ResolvedBackend::Windows(windows) => {
            plan.exclusive_claims.push(format!(
                "ssh-windows-repo:{}:{}",
                windows.host.as_deref().unwrap_or("?"),
                windows.repo_path
            ));
        }
        ResolvedBackend::Cloud(_) => plan.cloud_targets.push(target.name.clone()),
        ResolvedBackend::HostPool(pool) => {
            let mut requires = pool.requires.clone();
            requires.sort();
            requires.dedup();
            push_host_pool_demand(
                plan,
                HostPoolDemand {
                    pool_name: pool.pool_name.clone(),
                    capability_key: capability_key(&requires),
                    requires,
                    slots: 1,
                },
            );
        }
        ResolvedBackend::Fallback(chain) => {
            if let Some(primary) = chain.backends.first() {
                collect_backend_resource_demands(primary.target.as_ref(), plan, cwd);
            }
        }
    }
}

fn push_host_pool_demand(plan: &mut JobResourcePlan, demand: HostPoolDemand) {
    if let Some(existing) = plan.host_pools.iter_mut().find(|existing| {
        existing.pool_name == demand.pool_name
            && existing.capability_key == demand.capability_key
            && existing.requires == demand.requires
    }) {
        existing.slots = existing.slots.saturating_add(demand.slots);
        return;
    }
    plan.host_pools.push(demand);
}

fn default_host_pool_slots() -> u32 {
    1
}

fn capability_key(requires: &[String]) -> String {
    if requires.is_empty() {
        "*".to_owned()
    } else {
        requires.join("+")
    }
}

fn normalized_path_claim(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> QueueRequestResult<()> {
    let Some(parent) = path.parent() else {
        return Err(QueueRequestError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "queue request path has no parent",
        )));
    };
    fs::create_dir_all(parent)?;
    let temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&temp, value)?;
    temp.persist(path)
        .map_err(|error| QueueRequestError::Io(error.error))?;
    Ok(())
}

fn read_versioned_json<T>(path: &Path) -> QueueRequestResult<Option<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(QueueRequestError::Io(error)),
    };
    let value: Value = serde_json::from_str(&contents)?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .map(u32::try_from)
        .transpose()
        .map_err(|_| QueueRequestError::UnsupportedSchema { version: u32::MAX })?
        .unwrap_or_default();
    if version != QUEUED_EXECUTION_SCHEMA_VERSION {
        return Err(QueueRequestError::UnsupportedSchema { version });
    }
    Ok(Some(serde_json::from_value(value)?))
}

fn delete_if_present(path: &Path) -> QueueRequestResult<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(QueueRequestError::Io(error)),
    }
}

fn sweep_absent_older_than(
    dir: &Path,
    active_job_ids: &BTreeSet<String>,
    grace: Duration,
) -> QueueRequestResult<Vec<String>> {
    let mut removed = Vec::new();
    let now = SystemTime::now();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(job_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        if active_job_ids.contains(&job_id) {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified = metadata.modified()?;
        if now.duration_since(modified).unwrap_or(Duration::ZERO) < grace {
            continue;
        }
        fs::remove_file(&path)?;
        removed.push(job_id);
    }
    removed.sort();
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{
        HostPoolDemand, JobResourcePlan, QUEUED_EXECUTION_SCHEMA_VERSION, QueueOutcomeStore,
        QueueRequestError, QueueRequestStore, QueuedExecutionEnvelope, QueuedExecutionKind,
        QueuedExecutionOutcome, QueuedExecutionRequest,
    };
    use crate::executor::cloud::CloudTargetConfig;
    use crate::executor::dispatch::{
        FallbackBackend, FallbackTargetConfig, ResolvedBackend, ResolvedHostPoolConfig,
        ResolvedHostPoolMember, ResolvedTarget, ResolvedValidation,
    };
    use crate::executor::local::{LocalTargetConfig, LocalValidationConfig};
    use crate::executor::ssh::{SshTargetConfig, SshValidation};
    use crate::executor::ssh_windows::{WindowsTargetConfig, WindowsValidation};
    use crate::job::{Priority, ValidationMode};
    use crate::ship::{RunExecutionRequest, ShipExecutionRequest};
    use crate::ship_state::{DispatchedRun, ShipState};

    fn local_target() -> ResolvedTarget {
        local_target_with_name("mac", Some(PathBuf::from("/repo")))
    }

    fn local_target_with_name(name: &str, cwd: Option<PathBuf>) -> ResolvedTarget {
        let mut stages = BTreeMap::new();
        stages.insert("test".to_owned(), "cargo test".to_owned());
        ResolvedTarget {
            name: name.to_owned(),
            platform: "macos".to_owned(),
            backend_name: "local".to_owned(),
            warm_keepalive_seconds: 60,
            host: None,
            backend: ResolvedBackend::Local(LocalTargetConfig {
                name: name.to_owned(),
                platform: "macos".to_owned(),
                cwd,
                timeout_secs: 300,
            }),
            validation: ResolvedValidation::Local(LocalValidationConfig {
                command: None,
                stages,
                contract: None,
                prepared_state_enabled: true,
                allow_tree_drift: false,
            }),
            failure_parser: Some("auto".to_owned()),
        }
    }

    fn ssh_target(name: &str, host: &str, repo_path: &str) -> ResolvedTarget {
        ResolvedTarget {
            name: name.to_owned(),
            platform: "linux".to_owned(),
            backend_name: "ssh".to_owned(),
            warm_keepalive_seconds: 60,
            host: Some(host.to_owned()),
            backend: ResolvedBackend::Ssh(SshTargetConfig {
                name: name.to_owned(),
                platform: "linux".to_owned(),
                host: Some(host.to_owned()),
                repo_path: repo_path.to_owned(),
                ssh_options: Vec::new(),
                identity_file: None,
                remote_bundle_path: "/tmp/shipyard.bundle".to_owned(),
                local_repo_dir: None,
                timeout_secs: 300,
                bundle_upload_timeout_secs: 60,
                bundle_apply_timeout_secs: 60,
            }),
            validation: ResolvedValidation::Ssh {
                validation: SshValidation::Command("cargo test".to_owned()),
                contract: None,
            },
            failure_parser: None,
        }
    }

    fn windows_target(name: &str, host: &str, repo_path: &str) -> ResolvedTarget {
        ResolvedTarget {
            name: name.to_owned(),
            platform: "windows-x64".to_owned(),
            backend_name: "ssh-windows".to_owned(),
            warm_keepalive_seconds: 60,
            host: Some(host.to_owned()),
            backend: ResolvedBackend::Windows(WindowsTargetConfig {
                name: name.to_owned(),
                platform: "windows-x64".to_owned(),
                host: Some(host.to_owned()),
                repo_path: repo_path.to_owned(),
                ssh_options: Vec::new(),
                identity_file: None,
                remote_bundle_path: "shipyard.bundle".to_owned(),
                local_repo_dir: None,
                timeout_secs: 300,
                bundle_upload_timeout_secs: 60,
                bundle_apply_timeout_secs: 60,
                windows_vs_detect: true,
                windows_host_mutex: true,
                windows_host_mutex_name: "shipyard".to_owned(),
            }),
            validation: ResolvedValidation::Windows {
                validation: WindowsValidation::Command("cargo test".to_owned()),
                contract: None,
            },
            failure_parser: None,
        }
    }

    fn cloud_target(name: &str) -> ResolvedTarget {
        ResolvedTarget {
            name: name.to_owned(),
            platform: "linux".to_owned(),
            backend_name: "cloud".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: ResolvedBackend::Cloud(CloudTargetConfig {
                name: name.to_owned(),
                platform: "linux".to_owned(),
                workflow: "ci.yml".to_owned(),
                repository: None,
                runner_provider: None,
                runner_selector: None,
                runner_overrides: BTreeMap::new(),
                poll_interval_secs: 5,
                dispatch_settle_secs: 30,
                max_poll_secs: 300,
                failure_parser: None,
            }),
            validation: ResolvedValidation::Cloud,
            failure_parser: None,
        }
    }

    fn run_request() -> RunExecutionRequest {
        RunExecutionRequest {
            branch: "feat/run".to_owned(),
            sha: "abc123".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: true,
            resume_from: Some("test".to_owned()),
            targets: vec![local_target()],
        }
    }

    fn ship_request() -> ShipExecutionRequest {
        ShipExecutionRequest {
            pr: 42,
            repo: "danielraffel/shipyard".to_owned(),
            branch: "feat/ship".to_owned(),
            base_branch: "main".to_owned(),
            sha: "def456".to_owned(),
            commit_subject: "queue request store".to_owned(),
            pr_url: Some("https://github.com/danielraffel/shipyard/pull/42".to_owned()),
            pr_title: Some("Queue request store".to_owned()),
            mode: ValidationMode::Full,
            priority: Priority::High,
            warm_disabled: true,
            fail_fast: false,
            resume_from: None,
            advisory_targets: BTreeSet::from(["mac".to_owned()]),
            targets: vec![local_target()],
        }
    }

    #[test]
    fn round_trip_queued_run_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let envelope =
            QueuedExecutionEnvelope::from_run_request("job-run", "/work/repo", &run_request());

        store.save(&envelope).expect("save");
        let loaded = store.load("job-run").expect("load").expect("present");

        assert_eq!(loaded, envelope);
        assert_eq!(loaded.kind, QueuedExecutionKind::Run);
        assert!(matches!(loaded.request, QueuedExecutionRequest::Run(_)));
        assert_eq!(loaded.to_run_request().expect("restore run"), run_request());
    }

    #[test]
    fn sweep_absent_request_and_outcome_envelopes_respects_active_jobs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let request_store = QueueRequestStore::new(temp.path()).expect("request store");
        let outcome_store = QueueOutcomeStore::new(temp.path()).expect("outcome store");
        let request = run_request();
        let active =
            QueuedExecutionEnvelope::from_run_request("active-job", "/work/repo", &request);
        let stale = QueuedExecutionEnvelope::from_run_request("stale-job", "/work/repo", &request);
        request_store.save(&active).expect("save active request");
        request_store.save(&stale).expect("save stale request");
        outcome_store
            .save(&QueuedExecutionOutcome::run("active-job"))
            .expect("save active outcome");
        outcome_store
            .save(&QueuedExecutionOutcome::run("stale-job"))
            .expect("save stale outcome");

        let active_job_ids = BTreeSet::from(["active-job".to_owned()]);

        assert_eq!(
            request_store
                .sweep_absent_older_than(&active_job_ids, Duration::ZERO)
                .expect("sweep requests"),
            vec!["stale-job".to_owned()]
        );
        assert_eq!(
            outcome_store
                .sweep_absent_older_than(&active_job_ids, Duration::ZERO)
                .expect("sweep outcomes"),
            vec!["stale-job".to_owned()]
        );
        assert!(
            request_store
                .load("active-job")
                .expect("load active")
                .is_some()
        );
        assert!(
            request_store
                .load("stale-job")
                .expect("load stale")
                .is_none()
        );
        assert!(
            outcome_store
                .load("active-job")
                .expect("load active")
                .is_some()
        );
        assert!(
            outcome_store
                .load("stale-job")
                .expect("load stale")
                .is_none()
        );
    }

    #[test]
    fn round_trip_queued_ship_request_and_outcome() {
        let temp = tempfile::tempdir().expect("tempdir");
        let request_store = QueueRequestStore::new(temp.path()).expect("request store");
        let outcome_store = QueueOutcomeStore::new(temp.path()).expect("outcome store");
        let request = ship_request();
        let envelope =
            QueuedExecutionEnvelope::from_ship_request("job-ship", "/work/repo", &request);
        request_store.save(&envelope).expect("save request");

        let state = ShipState {
            pr: request.pr,
            repo: request.repo.clone(),
            branch: request.branch.clone(),
            base_branch: request.base_branch.clone(),
            head_sha: request.sha.clone(),
            policy_signature: "policy".to_owned(),
            pr_url: request.pr_url.clone().unwrap_or_default(),
            pr_title: request.pr_title.clone().unwrap_or_default(),
            commit_subject: request.commit_subject.clone(),
            created_at: envelope.created_at,
            updated_at: envelope.created_at,
            dispatched_runs: Vec::<DispatchedRun>::new(),
            evidence_snapshot: BTreeMap::new(),
            attempt: 1,
            schema_version: crate::ship_state::SHIP_STATE_SCHEMA_VERSION,
        };
        let outcome = QueuedExecutionOutcome::ship("job-ship", request.pr, state, true);
        outcome_store.save(&outcome).expect("save outcome");

        assert_eq!(
            request_store.load("job-ship").expect("load request"),
            Some(envelope)
        );
        assert_eq!(
            outcome_store.load("job-ship").expect("load outcome"),
            Some(outcome)
        );
        let restored = request_store
            .load("job-ship")
            .expect("load request")
            .expect("present")
            .to_ship_request()
            .expect("restore ship");
        assert_eq!(restored, request);
    }

    #[test]
    fn restore_queued_request_preserves_nested_host_pool_and_fallback_targets() {
        let host_pool_member = ResolvedHostPoolMember {
            id: "mac-a".to_owned(),
            target: Box::new(local_target_with_name(
                "mac-a",
                Some(PathBuf::from("/pool/repo")),
            )),
            label: "Mac A".to_owned(),
            profile_label: "macos arm64".to_owned(),
            max_concurrency: 1,
            capabilities: vec!["macos".to_owned(), "arm64".to_owned()],
        };
        let host_pool = ResolvedTarget {
            name: "pool-mac".to_owned(),
            platform: "macos".to_owned(),
            backend_name: "host_pool".to_owned(),
            warm_keepalive_seconds: 30,
            host: None,
            backend: ResolvedBackend::HostPool(ResolvedHostPoolConfig {
                pool_name: "local_macs".to_owned(),
                strategy: "ordered".to_owned(),
                lease_stale_seconds: 180,
                heartbeat_interval_seconds: 15,
                requires: vec!["macos".to_owned(), "arm64".to_owned()],
                members: vec![host_pool_member],
            }),
            validation: ResolvedValidation::HostPool,
            failure_parser: None,
        };
        let fallback = ResolvedTarget {
            name: "fallback".to_owned(),
            platform: "linux".to_owned(),
            backend_name: "fallback".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: ResolvedBackend::Fallback(FallbackTargetConfig {
                backends: vec![
                    FallbackBackend {
                        target: Box::new(ssh_target("ssh-a", "mac-a", "/repo-a")),
                        label: "ssh-a".to_owned(),
                        profile_label: "primary".to_owned(),
                        capabilities: vec!["macos".to_owned()],
                    },
                    FallbackBackend {
                        target: Box::new(cloud_target("linux-cloud")),
                        label: "cloud".to_owned(),
                        profile_label: "fallback".to_owned(),
                        capabilities: vec!["linux".to_owned()],
                    },
                ],
                requires: vec!["macos".to_owned()],
                heartbeat_stale_secs: 120,
            }),
            validation: ResolvedValidation::Fallback,
            failure_parser: Some("auto".to_owned()),
        };
        let mut request = run_request();
        request.targets = vec![
            host_pool,
            fallback,
            windows_target("win", "win-host", r"C:\repo"),
        ];
        let envelope = QueuedExecutionEnvelope::from_run_request("job-run", "/work/repo", &request);

        let restored = envelope.to_run_request().expect("restore run");

        assert_eq!(restored, request);
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        std::fs::write(
            store.path_for("future"),
            serde_json::to_string_pretty(&json!({
                "schema_version": 999,
                "job_id": "future"
            }))
            .expect("json"),
        )
        .expect("write");

        let error = store.load("future").expect_err("schema rejected");
        assert!(matches!(
            error,
            QueueRequestError::UnsupportedSchema { version: 999 }
        ));
    }

    #[test]
    fn request_snapshot_contains_no_token_fields() {
        fn assert_no_token_keys(value: &Value) {
            match value {
                Value::Object(object) => {
                    for (key, value) in object {
                        let key = key.to_ascii_lowercase();
                        assert!(!key.contains("token"), "{key}");
                        assert!(!key.contains("secret"), "{key}");
                        assert!(!key.contains("private_key"), "{key}");
                        assert_no_token_keys(value);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        assert_no_token_keys(value);
                    }
                }
                _ => {}
            }
        }

        let envelope =
            QueuedExecutionEnvelope::from_ship_request("job-ship", "/work/repo", &ship_request());
        let value = serde_json::to_value(&envelope).expect("serialize");

        assert_no_token_keys(&value);
    }

    #[test]
    fn outcome_constructor_sets_current_schema() {
        let outcome = QueuedExecutionOutcome::run("job-run");
        assert_eq!(outcome.schema_version(), QUEUED_EXECUTION_SCHEMA_VERSION);
        assert_eq!(outcome.job_id(), "job-run");
    }

    #[test]
    fn run_resource_plan_claims_local_evidence_and_warm_pool() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut request = run_request();
        request.targets = vec![local_target_with_name("mac", None)];

        let plan = JobResourcePlan::from_run_request(temp.path(), &request);

        assert_eq!(plan.targets, ["mac"]);
        assert!(plan.cloud_targets.is_empty());
        assert!(plan.host_pools.is_empty());
        assert!(plan.exclusive_claims.contains(&format!(
            "local-cwd:{}",
            temp.path().canonicalize().expect("canonical").display()
        )));
        assert!(
            plan.exclusive_claims
                .contains(&"evidence:feat/run:mac".to_owned())
        );
        assert!(plan.exclusive_claims.contains(&"warm:mac:local".to_owned()));
    }

    #[test]
    fn ship_resource_plan_claims_pr_state() {
        let request = ship_request();

        let plan = JobResourcePlan::from_ship_request(Path::new("/work/repo"), &request);

        assert!(
            plan.exclusive_claims
                .contains(&"ship-state:danielraffel/shipyard:pr-42".to_owned())
        );
        assert!(
            plan.exclusive_claims
                .contains(&"evidence:feat/ship:mac".to_owned())
        );
    }

    #[test]
    fn resource_plan_claims_ssh_and_windows_repos() {
        let mut request = run_request();
        request.targets = vec![
            ssh_target("linux", "mac-studio", "/Users/shipyard/work/repo"),
            windows_target("windows", "win-builder", r"C:\repo"),
        ];

        let plan = JobResourcePlan::from_run_request(Path::new("/work/repo"), &request);

        assert!(
            plan.exclusive_claims
                .contains(&"ssh-repo:mac-studio:/Users/shipyard/work/repo".to_owned())
        );
        assert!(
            plan.exclusive_claims
                .contains(&r"ssh-windows-repo:win-builder:C:\repo".to_owned())
        );
        assert!(
            plan.exclusive_claims
                .contains(&"warm:linux:mac-studio".to_owned())
        );
        assert!(
            plan.exclusive_claims
                .contains(&"warm:windows:win-builder".to_owned())
        );
    }

    #[test]
    fn cloud_resource_plan_has_no_exclusive_cloud_serial_claim() {
        let mut request = run_request();
        request.targets = vec![cloud_target("ubuntu")];

        let plan = JobResourcePlan::from_run_request(Path::new("/work/repo"), &request);

        assert_eq!(plan.cloud_targets, ["ubuntu"]);
        assert!(
            plan.exclusive_claims
                .contains(&"evidence:feat/run:ubuntu".to_owned())
        );
        assert!(
            !plan
                .exclusive_claims
                .iter()
                .any(|claim| claim.contains("cloud"))
        );
    }

    #[test]
    fn host_pool_resource_plan_uses_pool_demand_not_member_claims() {
        let member = ssh_target("member-a", "mac-a", "/Users/shipyard/work/repo");
        let mut request = run_request();
        let target = ResolvedTarget {
            name: "mac-pool".to_owned(),
            platform: "macos".to_owned(),
            backend_name: "host-pool".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: ResolvedBackend::HostPool(ResolvedHostPoolConfig {
                pool_name: "studio".to_owned(),
                strategy: "least-recently-used".to_owned(),
                lease_stale_seconds: 300,
                heartbeat_interval_seconds: 30,
                requires: vec!["xcode".to_owned(), "arm64".to_owned(), "xcode".to_owned()],
                members: vec![ResolvedHostPoolMember {
                    id: "mac-a".to_owned(),
                    target: Box::new(member),
                    label: "mac-a".to_owned(),
                    profile_label: "mac-a".to_owned(),
                    max_concurrency: 1,
                    capabilities: vec!["arm64".to_owned(), "xcode".to_owned()],
                }],
            }),
            validation: ResolvedValidation::HostPool,
            failure_parser: None,
        };
        request.targets = vec![target.clone(), target];

        let plan = JobResourcePlan::from_run_request(Path::new("/work/repo"), &request);

        assert_eq!(
            plan.host_pools,
            [HostPoolDemand {
                pool_name: "studio".to_owned(),
                requires: vec!["arm64".to_owned(), "xcode".to_owned()],
                slots: 2,
                capability_key: "arm64+xcode".to_owned(),
            }]
        );
        assert!(
            !plan
                .exclusive_claims
                .iter()
                .any(|claim| claim.contains("mac-a") || claim.contains("/Users/shipyard"))
        );
    }

    #[test]
    fn fallback_resource_plan_claims_only_primary_backend() {
        let mut request = run_request();
        request.targets = vec![ResolvedTarget {
            name: "fallback".to_owned(),
            platform: "macos".to_owned(),
            backend_name: "fallback".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: ResolvedBackend::Fallback(FallbackTargetConfig {
                backends: vec![
                    FallbackBackend {
                        target: Box::new(ssh_target("primary", "mac-a", "/repo-a")),
                        label: "primary".to_owned(),
                        profile_label: "primary".to_owned(),
                        capabilities: vec!["arm64".to_owned()],
                    },
                    FallbackBackend {
                        target: Box::new(ssh_target("secondary", "mac-b", "/repo-b")),
                        label: "secondary".to_owned(),
                        profile_label: "secondary".to_owned(),
                        capabilities: vec!["arm64".to_owned()],
                    },
                ],
                requires: vec!["arm64".to_owned()],
                heartbeat_stale_secs: 300,
            }),
            validation: ResolvedValidation::Fallback,
            failure_parser: None,
        }];

        let plan = JobResourcePlan::from_run_request(Path::new("/work/repo"), &request);

        assert!(
            plan.exclusive_claims
                .contains(&"ssh-repo:mac-a:/repo-a".to_owned())
        );
        assert!(
            !plan
                .exclusive_claims
                .contains(&"ssh-repo:mac-b:/repo-b".to_owned())
        );
    }
}
