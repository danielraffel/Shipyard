//! Durable queued execution request and outcome stores.
//!
//! Queue request snapshots are owned by the queue layer instead of making
//! executor runtime structs a serde compatibility contract.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::LoadedConfig;
use crate::evidence::{
    canonical_repository, evidence_resource_claim, run_evidence_scope, ship_evidence_scope,
};
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
#[cfg(any(unix, test))]
use crate::record_identity::is_exact_lower_hex_git_sha;
#[cfg(any(unix, test))]
use crate::record_identity::is_valid_repository_slug;
use crate::ship::{RunExecutionRequest, ShipExecutionRequest};
#[cfg(any(unix, test))]
use crate::ship_state::SHIP_STATE_SCHEMA_VERSION;
use crate::ship_state::ShipState;
use crate::warm_pool::{is_backend_eligible, warm_host_key};

/// Current queued-execution schema.
pub const QUEUED_EXECUTION_SCHEMA_VERSION: u32 = 4;
const LEGACY_QUEUED_EXECUTION_SCHEMA_VERSION: u32 = 1;
const TRUSTED_ENVIRONMENT_QUEUED_EXECUTION_SCHEMA_VERSION: u32 = 2;
const PREVIOUS_QUEUED_EXECUTION_SCHEMA_VERSION: u32 = 3;
const MAX_SHIP_POST_VALIDATION_DETAIL_BYTES: usize = 1_200;

/// Durable submitter ownership. Running ownership is derived by admitting
/// only the matching executor class; it is never inferred from optional
/// checkout provenance.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedExecutionOwner {
    /// Request predates explicit ownership. Its signed configuration
    /// provenance distinguishes legacy daemon submissions from foreground
    /// submissions without changing the on-disk schema version.
    #[default]
    LegacyUnspecified,
    /// An attached explicit `--foreground` drain owns execution.
    Foreground,
    /// The durable daemon supervisor owns execution.
    Daemon,
}

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
    /// A valid experimental authority request was recognized but cannot be
    /// projected into an executable queue envelope.
    #[cfg(feature = "experimental-authority-v5")]
    ExperimentalAuthorityRefused,
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
            #[cfg(feature = "experimental-authority-v5")]
            Self::ExperimentalAuthorityRefused => {
                formatter.write_str("experimental authority request refused")
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
            #[cfg(feature = "experimental-authority-v5")]
            Self::ExperimentalAuthorityRefused => None,
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

/// Exclusive per-request mutation fence shared by submission, recovery, and
/// receiptless orphan reconciliation.
pub(crate) struct QueueRequestMutationGuard(File);

impl Drop for QueueRequestMutationGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

impl QueueRequestStore {
    /// Open a request store rooted at `<state_dir>/queue/requests`.
    pub fn new(state_dir: impl Into<PathBuf>) -> io::Result<Self> {
        let path = state_dir.into().join("queue").join("requests");
        ensure_request_directory(&path)?;
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

    /// Exclude every cooperative writer for one exact immutable request.
    pub(crate) fn acquire_mutation_lock(
        &self,
        job_id: &str,
    ) -> QueueRequestResult<QueueRequestMutationGuard> {
        validate_job_id(job_id)?;
        let lock_dir = self.path.join(".locks");
        crate::writer_domain_lease::ensure_protected_dir_all(&lock_dir)?;
        let lock_path = lock_dir.join(format!("{job_id}.lock"));
        let writer_domain = crate::writer_domain_lease::acquire_for_protected_creation(&lock_path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        drop(writer_domain);
        FileExt::lock_exclusive(&file)?;
        Ok(QueueRequestMutationGuard(file))
    }

    /// Save one request envelope atomically.
    pub fn save(&self, envelope: &QueuedExecutionEnvelope) -> QueueRequestResult<()> {
        if envelope.schema_version != QUEUED_EXECUTION_SCHEMA_VERSION {
            return Err(QueueRequestError::UnsupportedSchema {
                version: envelope.schema_version,
            });
        }
        let _mutation = self.acquire_mutation_lock(&envelope.job_id)?;
        write_json_atomic(&self.path_for(&envelope.job_id), envelope)
    }

    /// Store one request envelope and fsync both file and containing directory
    /// before a dependent queue commit.
    pub fn save_durable(&self, envelope: &QueuedExecutionEnvelope) -> QueueRequestResult<()> {
        if envelope.schema_version != QUEUED_EXECUTION_SCHEMA_VERSION {
            return Err(QueueRequestError::UnsupportedSchema {
                version: envelope.schema_version,
            });
        }
        let _mutation = self.acquire_mutation_lock(&envelope.job_id)?;
        write_json_atomic_durable(&self.path_for(&envelope.job_id), envelope)
    }

    /// Load one request envelope.
    pub fn load(&self, job_id: &str) -> QueueRequestResult<Option<QueuedExecutionEnvelope>> {
        read_queue_request_json(&self.path_for(job_id))?
            .map(upgrade_legacy_request)
            .transpose()
    }

    /// Delete one request envelope, if present.
    pub fn delete(&self, job_id: &str) -> QueueRequestResult<bool> {
        let _mutation = self.acquire_mutation_lock(job_id)?;
        delete_if_present(&self.path_for(job_id))
    }

    /// Load every durable request envelope. Any malformed entry fails the
    /// whole scan closed; recovery must never silently route around unknown
    /// ownership.
    pub fn list(&self) -> QueueRequestResult<Vec<QueuedExecutionEnvelope>> {
        let mut envelopes: Vec<QueuedExecutionEnvelope> = Vec::new();
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if let Some(envelope) = read_queue_request_json(&path)? {
                envelopes.push(upgrade_legacy_request(envelope)?);
            }
        }
        envelopes.sort_by_key(|envelope| envelope.created_at);
        Ok(envelopes)
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
        crate::writer_domain_lease::ensure_protected_dir_all(&path)?;
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
    /// Executor class allowed to transition this request to running.
    #[serde(default)]
    pub execution_owner: QueuedExecutionOwner,
    /// Immutable checkout identity captured by the submitting process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ExecutionProvenance>,
    /// Scheduler-facing resource plan snapshot.
    pub resource_plan: JobResourcePlan,
    /// Resolved execution request.
    pub request: QueuedExecutionRequest,
}

impl QueuedExecutionEnvelope {
    /// Whether this request is controller-only metadata authority and therefore
    /// consumes no native target-worker capacity.
    #[must_use]
    pub(crate) fn is_metadata_authority_controller(&self) -> bool {
        matches!(
            &self.request,
            QueuedExecutionRequest::Ship(request)
                if request.metadata_authority_receipt.is_some()
                    && request.targets.is_empty()
                    && self.resource_plan.targets.is_empty()
                    && self.resource_plan.cloud_targets.is_empty()
                    && self.resource_plan.host_pools.is_empty()
                    && self.resource_plan.vm_slots.is_empty()
        )
    }

    /// Whether this request carries the configuration provenance required for
    /// daemon-owned execution. Foreground requests deliberately omit that
    /// signature; missing or unreadable envelopes must be treated separately
    /// as unknown ownership and preserved fail closed.
    #[must_use]
    pub fn is_daemon_owned(&self) -> bool {
        self.execution_owner == QueuedExecutionOwner::Daemon
    }

    /// Whether the daemon may admit this request, including a request written
    /// by the pre-ownership daemon. Only daemon submission captured a resolved
    /// configuration signature in that format.
    #[must_use]
    pub fn is_daemon_admissible(&self) -> bool {
        matches!(
            self.execution_owner,
            QueuedExecutionOwner::Daemon | QueuedExecutionOwner::LegacyUnspecified
        ) && self
            .provenance
            .as_ref()
            .and_then(|provenance| provenance.config_signature.as_ref())
            .is_some()
    }

    /// Whether a cooperative foreground drain may execute or recover this
    /// request, including requests written before explicit ownership existed.
    #[must_use]
    pub fn is_foreground_owned(&self) -> bool {
        matches!(self.execution_owner, QueuedExecutionOwner::Foreground)
            || (self.execution_owner == QueuedExecutionOwner::LegacyUnspecified
                && self
                    .provenance
                    .as_ref()
                    .and_then(|provenance| provenance.config_signature.as_ref())
                    .is_none())
    }

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
            execution_owner: QueuedExecutionOwner::Foreground,
            provenance: ExecutionProvenance::capture(&cwd, None, &request.sha),
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
            execution_owner: QueuedExecutionOwner::Foreground,
            provenance: ExecutionProvenance::capture(&cwd, Some(&request.repo), &request.sha),
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

    /// Recover the stable queue supersedence identity for persisted envelopes
    /// written before `Job::workload_scope` existed.
    #[must_use]
    pub(crate) fn workload_scope(&self) -> String {
        match &self.request {
            QueuedExecutionRequest::Run(_) => run_workload_scope(&self.cwd),
            QueuedExecutionRequest::Ship(request) => {
                format!(
                    "ship:{}:pr-{}",
                    canonical_repository(&request.repo),
                    request.pr
                )
            }
        }
    }
}

/// Stable repository-checkout identity for repo-neutral `run` supersedence.
#[must_use]
pub(crate) fn run_workload_scope(cwd: &Path) -> String {
    run_evidence_scope(cwd)
}

/// Immutable checkout identity required before a daemon-owned worker may run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionProvenance {
    /// Canonical submitting working directory.
    pub canonical_cwd: PathBuf,
    /// Canonical Git repository root.
    pub repo_root: PathBuf,
    /// Repository slug when the request performs GitHub operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_slug: Option<String>,
    /// Exact submitted Git HEAD.
    pub head_sha: String,
    /// Signature of HEAD plus tracked and untracked working-tree contents.
    pub tree_signature: String,
    /// Signature of the resolved layered Shipyard configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_signature: Option<String>,
}

impl ExecutionProvenance {
    pub(crate) fn capture(cwd: &Path, repo_slug: Option<&str>, expected_sha: &str) -> Option<Self> {
        let canonical_cwd = fs::canonicalize(cwd).ok()?;
        let repo_root = git_output(&canonical_cwd, &["rev-parse", "--show-toplevel"])?;
        let repo_root = fs::canonicalize(repo_root.trim()).ok()?;
        let head_sha = git_output(&canonical_cwd, &["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        if head_sha != expected_sha {
            return None;
        }
        if let Some(expected_repo) = repo_slug {
            let remote = git_output(&canonical_cwd, &["remote", "get-url", "origin"])?;
            if parse_repo_slug(remote.trim()).as_deref() != Some(expected_repo) {
                return None;
            }
        }
        Some(Self {
            canonical_cwd,
            repo_root,
            repo_slug: repo_slug.map(str::to_owned),
            head_sha,
            tree_signature: crate::tree_drift::compute_signature(cwd)?,
            config_signature: None,
        })
    }

    /// Capture checkout identity plus the exact resolved configuration used by
    /// an unattended submission.
    pub(crate) fn capture_with_config(
        cwd: &Path,
        repo_slug: Option<&str>,
        expected_sha: &str,
        config: &LoadedConfig,
    ) -> Option<Self> {
        let mut provenance = Self::capture(cwd, repo_slug, expected_sha)?;
        provenance.config_signature = config_signature(config);
        provenance.config_signature.as_ref()?;
        Some(provenance)
    }

    /// Fail closed when a delayed worker no longer sees the submitted checkout.
    pub fn validate(&self, cwd: &Path) -> QueueRequestResult<()> {
        let canonical_cwd = fs::canonicalize(cwd)
            .map_err(|error| invalid_snapshot(format!("submitted cwd is unavailable: {error}")))?;
        if canonical_cwd != self.canonical_cwd {
            return Err(invalid_snapshot("submitted cwd identity changed"));
        }
        let repo_root = git_output(cwd, &["rev-parse", "--show-toplevel"])
            .and_then(|path| fs::canonicalize(path.trim()).ok())
            .ok_or_else(|| invalid_snapshot("submitted cwd is no longer a Git checkout"))?;
        if repo_root != self.repo_root {
            return Err(invalid_snapshot("submitted repository root changed"));
        }
        let head = git_output(cwd, &["rev-parse", "HEAD"])
            .map(|head| head.trim().to_owned())
            .ok_or_else(|| invalid_snapshot("submitted Git HEAD is unreadable"))?;
        if head != self.head_sha {
            return Err(invalid_snapshot(format!(
                "submitted Git HEAD drifted from {} to {head}",
                self.head_sha
            )));
        }
        if let Some(expected_repo) = &self.repo_slug {
            let remote = git_output(cwd, &["remote", "get-url", "origin"])
                .and_then(|remote| parse_repo_slug(remote.trim()))
                .ok_or_else(|| invalid_snapshot("submitted Git origin is unreadable"))?;
            if &remote != expected_repo {
                return Err(invalid_snapshot(format!(
                    "submitted repository changed from {expected_repo} to {remote}"
                )));
            }
        }
        let signature = crate::tree_drift::compute_signature(cwd)
            .ok_or_else(|| invalid_snapshot("submitted working tree is unreadable"))?;
        if signature != self.tree_signature {
            return Err(invalid_snapshot(
                "submitted working tree changed before execution",
            ));
        }
        Ok(())
    }

    /// Validate checkout identity and the resolved layered configuration.
    pub(crate) fn validate_with_config(
        &self,
        cwd: &Path,
        config: &LoadedConfig,
    ) -> QueueRequestResult<()> {
        self.validate(cwd)?;
        let expected = self
            .config_signature
            .as_ref()
            .ok_or_else(|| invalid_snapshot("request lacks unattended configuration provenance"))?;
        let current = config_signature(config)
            .ok_or_else(|| invalid_snapshot("resolved configuration cannot be signed"))?;
        if &current != expected {
            return Err(invalid_snapshot(
                "resolved Shipyard configuration changed before execution",
            ));
        }
        Ok(())
    }
}

fn config_signature(config: &LoadedConfig) -> Option<String> {
    // Queue snapshots already persist the resolved repository environment in
    // each target. Excluding the mutable machine-global source table lets a
    // delayed worker execute that immutable snapshot while every other policy
    // change remains fenced by this signature.
    let mut signed = config.data.clone();
    signed.remove("repository_environment");
    let serialized = toml::to_string(&signed).ok()?;
    Some(hex::encode(Sha256::digest(serialized.as_bytes())))
}

fn parse_repo_slug(remote: &str) -> Option<String> {
    let path = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("https://github.com/"))?;
    let slug = path.trim_end_matches('/').trim_end_matches(".git");
    (slug.split('/').count() == 2).then(|| slug.to_owned())
}

fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).to_string())
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
#[allow(clippy::large_enum_variant)] // Stable schema keeps ship payload inline for backward-compatible v1/v2 decoding.
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
    /// Adopt the current head SHA on recorded-state drift (amend/force-push),
    /// clearing prior evidence so the new head re-validates. See Shipyard #346.
    #[serde(default)]
    pub adopt_head: bool,
    /// Exact trusted authority for a ship with no native validation targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_authority_receipt: Option<crate::metadata_authority::MetadataAuthorityReceipt>,
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
            adopt_head: request.adopt_head,
            metadata_authority_receipt: request.metadata_authority_receipt.clone(),
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
            adopt_head: self.adopt_head,
            pr_snapshot_file: None,
            metadata_authority_receipt: self.metadata_authority_receipt.clone(),
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
    /// VM-slot demands, currently used for local macOS VM admission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vm_slots: Vec<VmSlotDemand>,
}

impl JobResourcePlan {
    /// Build a resource plan from resolved targets.
    #[must_use]
    pub fn from_targets(targets: &[ResolvedTarget]) -> Self {
        Self::from_targets_with_context("", Path::new("."), None, "legacy", targets)
    }

    /// Build a resource plan for a run request.
    #[must_use]
    pub fn from_run_request(cwd: &Path, request: &RunExecutionRequest) -> Self {
        Self::from_targets_with_context(
            &request.branch,
            cwd,
            None,
            &run_evidence_scope(cwd),
            &request.targets,
        )
    }

    /// Build a resource plan for a ship request.
    #[must_use]
    pub fn from_ship_request(cwd: &Path, request: &ShipExecutionRequest) -> Self {
        Self::from_targets_with_context(
            &request.branch,
            cwd,
            Some((&request.repo, request.pr)),
            &ship_evidence_scope(&request.repo, request.pr, cwd),
            &request.targets,
        )
    }

    fn from_targets_with_context(
        branch: &str,
        cwd: &Path,
        ship_scope: Option<(&str, u64)>,
        evidence_scope: &str,
        targets: &[ResolvedTarget],
    ) -> Self {
        let mut plan = Self {
            targets: targets.iter().map(|target| target.name.clone()).collect(),
            exclusive_claims: Vec::new(),
            cloud_targets: Vec::new(),
            host_pools: Vec::new(),
            vm_slots: Vec::new(),
        };
        if let Some((repo, pr)) = ship_scope {
            plan.exclusive_claims
                .push(format!("ship-state:{}:pr-{pr}", canonical_repository(repo)));
        }
        for target in targets {
            if !branch.is_empty() {
                plan.exclusive_claims.push(evidence_resource_claim(
                    evidence_scope,
                    branch,
                    &target.name,
                ));
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
        plan.vm_slots
            .sort_by(|left, right| left.key.cmp(&right.key));
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

/// One VM-slot demand.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct VmSlotDemand {
    /// Stable slot key. Today `macos` is the only slot-capped OS.
    pub key: String,
    /// Number of VM slots required by this logical target.
    #[serde(default = "default_vm_slots")]
    pub slots: u32,
}

/// Durable resolved target snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedResolvedTarget {
    /// Logical target name.
    pub name: String,
    /// Typed build configuration whose execution this target evidences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_build_type: Option<String>,
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
            validation_build_type: target.validation_build_type.clone(),
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

/// Digest the executed target's typed build and complete validation command contract.
#[must_use]
pub fn validation_contract_digest(target: &ResolvedTarget) -> Option<String> {
    let build_type = target.validation_build_type.as_deref()?;
    let mut validation = QueuedValidationSnapshot::from(&target.validation);
    if let QueuedValidationSnapshot::Local(local) = &mut validation {
        // Contract identity is portable across eligible hosts: requested
        // variable names are policy, while resolved absolute values are a
        // machine-local execution snapshot. Prepared-state reuse binds the
        // values separately at execution time.
        local.environment.clear();
        local.integration_cleanup = None;
    }
    let payload = serde_json::to_vec(&(build_type, validation)).ok()?;
    Some(format!("{:x}", Sha256::digest(payload)))
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
    /// Names requested from the trusted machine-global project environment.
    #[serde(default)]
    pub machine_environment: Vec<String>,
    /// Resolved trusted values snapshotted for daemon-owned execution.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Exact stale-integration checkout custody restored by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_cleanup:
        Option<Box<crate::changed_surface::integration_checkout::IntegrationCheckoutSnapshot>>,
}

impl From<&LocalValidationConfig> for QueuedLocalValidation {
    fn from(validation: &LocalValidationConfig) -> Self {
        Self {
            command: validation.command.clone(),
            stages: validation.stages.clone(),
            contract: validation.contract.as_ref().map(QueuedContract::from),
            prepared_state_enabled: validation.prepared_state_enabled,
            allow_tree_drift: validation.allow_tree_drift,
            machine_environment: validation.machine_environment.clone(),
            environment: validation.environment.clone(),
            integration_cleanup: validation
                .integration_cleanup
                .as_ref()
                .map(|checkout| Box::new(checkout.snapshot())),
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

/// Typed result of the post-validation merge/readiness phase.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedShipDispositionKind {
    /// The exact submitted pull-request head merged while validation was running.
    AlreadyMerged,
    /// One or more locally supervised validation targets failed.
    ValidationFailed,
    /// The pull request merged after validation.
    Merged,
    /// Validation passed but the downstream merge request was rejected.
    GreenNotMerged,
    /// Validation passed and deterministic merge readiness is still pending.
    GreenPendingMergeReadiness,
    /// Validation passed but the scoped state needed for readiness disappeared.
    GreenValidationStateMissing,
    /// Validation passed and a required-check failure was classified as flaky.
    GreenNotMergedFlakyRequired,
    /// Validation passed but Shipyard's merge client produced an invalid request.
    GreenNotMergedClientDefect,
    /// Automatic merge is unavailable under the live governance boundary.
    GreenAutomaticMergeRefused,
    /// Validation passed for an immutable head that the live PR superseded.
    GreenNotMergedHeadSuperseded,
    /// Validation completed, but deterministic post-validation handling failed.
    PostValidationOperationalFailure,
}

/// Durable post-validation disposition, kept separate from validation proof.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedShipDisposition {
    /// Typed merge-readiness/merge result.
    pub kind: QueuedShipDispositionKind,
    /// Worker exit code associated with this disposition.
    pub exit_code: u8,
    /// Bounded operational or readiness detail, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl QueuedShipDisposition {
    /// Build a bounded durable post-validation disposition.
    #[must_use]
    pub fn new(kind: QueuedShipDispositionKind, exit_code: u8, detail: Option<&str>) -> Self {
        Self {
            kind,
            exit_code,
            detail: detail.and_then(bounded_ship_post_validation_detail),
        }
    }
}

fn bounded_ship_post_validation_detail(detail: &str) -> Option<String> {
    let mut sanitized = detail
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if sanitized.len() > MAX_SHIP_POST_VALIDATION_DETAIL_BYTES {
        let mut boundary = MAX_SHIP_POST_VALIDATION_DETAIL_BYTES;
        while !sanitized.is_char_boundary(boundary) {
            boundary -= 1;
        }
        sanitized.truncate(boundary);
    }
    let sanitized = sanitized.trim().to_owned();
    (!sanitized.is_empty()).then_some(sanitized)
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
        /// Post-validation merge/readiness result. Legacy and validation-only
        /// outcomes omit this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        post_validation: Option<QueuedShipDisposition>,
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
            post_validation: None,
        }
    }

    /// Build a ship outcome with its separately-owned post-validation result.
    #[must_use]
    pub fn ship_with_post_validation(
        job_id: impl Into<String>,
        pr: u64,
        ship_state: ShipState,
        resumed_existing_state: bool,
        post_validation: QueuedShipDisposition,
    ) -> Self {
        Self::Ship {
            schema_version: QUEUED_EXECUTION_SCHEMA_VERSION,
            job_id: job_id.into(),
            pr,
            ship_state,
            resumed_existing_state,
            post_validation: Some(post_validation),
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

/// Apply the identity and nested-schema checks required before projection.
#[cfg(any(unix, test))]
pub(crate) fn validate_queued_execution_outcome(
    outcome: &QueuedExecutionOutcome,
) -> QueueRequestResult<()> {
    if !(LEGACY_QUEUED_EXECUTION_SCHEMA_VERSION..=QUEUED_EXECUTION_SCHEMA_VERSION)
        .contains(&outcome.schema_version())
    {
        return Err(QueueRequestError::UnsupportedSchema {
            version: outcome.schema_version(),
        });
    }
    validate_job_id(outcome.job_id())?;
    if let QueuedExecutionOutcome::Ship { pr, ship_state, .. } = outcome
        && (*pr == 0
            || ship_state.schema_version != SHIP_STATE_SCHEMA_VERSION
            || ship_state.pr != *pr
            || !is_valid_repository_slug(&ship_state.repo)
            || !is_exact_lower_hex_git_sha(&ship_state.head_sha)
            || ship_state.base_branch.is_empty())
    {
        return Err(invalid_snapshot(
            "queued ship outcome has invalid nested ship identity",
        ));
    }
    Ok(())
}

fn snapshot_targets(targets: &[ResolvedTarget]) -> Vec<QueuedResolvedTarget> {
    targets.iter().map(QueuedResolvedTarget::from).collect()
}

fn restore_targets(targets: &[QueuedResolvedTarget]) -> QueueRequestResult<Vec<ResolvedTarget>> {
    targets.iter().map(restore_target).collect()
}

fn restore_target(target: &QueuedResolvedTarget) -> QueueRequestResult<ResolvedTarget> {
    let backend = restore_backend(&target.backend)?;
    let validation = restore_validation(&target.validation)?;
    if let ResolvedValidation::Local(local_validation) = &validation
        && let Some(cleanup) = local_validation.integration_cleanup.as_deref()
    {
        let ResolvedBackend::Local(local_backend) = &backend else {
            return Err(invalid_snapshot(
                "integration checkout custody requires a local backend",
            ));
        };
        if local_backend.cwd.as_deref() != Some(cleanup.path.as_path()) {
            return Err(invalid_snapshot(
                "integration checkout custody does not match local execution cwd",
            ));
        }
    }
    Ok(ResolvedTarget {
        name: target.name.clone(),
        validation_build_type: target.validation_build_type.clone(),
        platform: target.platform.clone(),
        backend_name: target.backend_name.clone(),
        warm_keepalive_seconds: target.warm_keepalive_seconds,
        host: target.host.clone(),
        backend,
        validation,
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

fn restore_validation(
    validation: &QueuedValidationSnapshot,
) -> QueueRequestResult<ResolvedValidation> {
    Ok(match validation {
        QueuedValidationSnapshot::Local(validation) => {
            ResolvedValidation::Local(LocalValidationConfig {
                command: validation.command.clone(),
                stages: validation.stages.clone(),
                contract: validation.contract.as_ref().map(restore_contract),
                prepared_state_enabled: validation.prepared_state_enabled,
                allow_tree_drift: validation.allow_tree_drift,
                machine_environment: validation.machine_environment.clone(),
                environment: validation.environment.clone(),
                integration_cleanup: validation
                    .integration_cleanup
                    .as_ref()
                    .map(|snapshot| snapshot.restore().map(Box::new))
                    .transpose()
                    .map_err(|error| {
                        invalid_snapshot(format!("restore integration checkout: {error}"))
                    })?,
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
    })
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
            push_target_vm_slot_demand(target, plan);
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
    if matches!(
        target.backend,
        ResolvedBackend::Local(_) | ResolvedBackend::Ssh(_) | ResolvedBackend::Windows(_)
    ) {
        push_target_vm_slot_demand(target, plan);
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

fn default_vm_slots() -> u32 {
    1
}

fn push_target_vm_slot_demand(target: &ResolvedTarget, plan: &mut JobResourcePlan) {
    let Some(key) = vm_slot_key(&target.platform) else {
        return;
    };
    push_vm_slot_demand(plan, VmSlotDemand { key, slots: 1 });
}

fn push_vm_slot_demand(plan: &mut JobResourcePlan, demand: VmSlotDemand) {
    if let Some(existing) = plan
        .vm_slots
        .iter_mut()
        .find(|existing| existing.key == demand.key)
    {
        existing.slots = existing.slots.saturating_add(demand.slots);
        return;
    }
    plan.vm_slots.push(demand);
}

fn vm_slot_key(platform: &str) -> Option<String> {
    let os = platform
        .split(['-', '_'])
        .next()
        .unwrap_or(platform)
        .trim()
        .to_ascii_lowercase();
    if os == "macos" || os == "darwin" {
        Some("macos".to_owned())
    } else {
        None
    }
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

#[cfg(unix)]
fn protect_request_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // Keep one fallible cross-platform protection contract.
fn protect_request_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn ensure_request_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    let protected = {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|metadata| {
            metadata.is_dir() && metadata.permissions().mode() & 0o777 == 0o700
        })
    };
    #[cfg(not(unix))]
    let protected = path.is_dir();
    if protected {
        return Ok(());
    }
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
    fs::create_dir_all(path)?;
    protect_request_directory(path)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> QueueRequestResult<()> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
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

fn write_json_atomic_durable<T: Serialize>(path: &Path, value: &T) -> QueueRequestResult<()> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
    let Some(parent) = path.parent() else {
        return Err(QueueRequestError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "queue request path has no parent",
        )));
    };
    fs::create_dir_all(parent)?;
    let temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(temp.as_file(), value)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|error| QueueRequestError::Io(error.error))?;
    crate::log_retention::sync_parent_directory(path)?;
    Ok(())
}

#[cfg(not(feature = "experimental-authority-v5"))]
fn read_queue_request_json(path: &Path) -> QueueRequestResult<Option<QueuedExecutionEnvelope>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(QueueRequestError::Io(error)),
    };
    decode_queued_execution_request_bytes(contents.as_bytes()).map(Some)
}

#[cfg(feature = "experimental-authority-v5")]
fn read_queue_request_json(path: &Path) -> QueueRequestResult<Option<QueuedExecutionEnvelope>> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(QueueRequestError::Io(error)),
    };
    let authoritative_filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_snapshot("queue request filename must be valid UTF-8"))?;
    decode_queued_execution_request_bytes_with_filename(&contents, Some(authoritative_filename))
        .map(Some)
}

/// Decode original queue-request bytes while enforcing the active reader
/// ceiling before a request reaches typed projection.
#[cfg(not(feature = "experimental-authority-v5"))]
pub(crate) fn decode_queued_execution_request_bytes(
    contents: &[u8],
) -> QueueRequestResult<QueuedExecutionEnvelope> {
    let value: Value = serde_json::from_slice(contents)?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .map(u32::try_from)
        .transpose()
        .map_err(|_| QueueRequestError::UnsupportedSchema { version: u32::MAX })?
        .unwrap_or_default();
    if !(LEGACY_QUEUED_EXECUTION_SCHEMA_VERSION..=QUEUED_EXECUTION_SCHEMA_VERSION)
        .contains(&version)
    {
        return Err(QueueRequestError::UnsupportedSchema { version });
    }
    Ok(serde_json::from_value(value)?)
}

#[cfg(not(feature = "experimental-authority-v5"))]
#[cfg(unix)]
pub(crate) fn decode_queued_execution_request_bytes_for_import(
    contents: &[u8],
    _authoritative_filename: &str,
) -> QueueRequestResult<QueuedExecutionEnvelope> {
    decode_queued_execution_request_bytes(contents)
}

#[cfg(feature = "experimental-authority-v5")]
// Retained as the raw decoder API for feature-gated readers and focused tests;
// store reads must use the filename-aware path below.
#[allow(dead_code)]
pub(crate) fn decode_queued_execution_request_bytes(
    contents: &[u8],
) -> QueueRequestResult<QueuedExecutionEnvelope> {
    decode_queued_execution_request_bytes_with_filename(contents, None)
}

#[cfg(feature = "experimental-authority-v5")]
pub(crate) fn decode_queued_execution_request_bytes_for_import(
    contents: &[u8],
    authoritative_filename: &str,
) -> QueueRequestResult<QueuedExecutionEnvelope> {
    decode_queued_execution_request_bytes_with_filename(contents, Some(authoritative_filename))
}

#[cfg(feature = "experimental-authority-v5")]
fn decode_queued_execution_request_bytes_with_filename(
    contents: &[u8],
    authoritative_filename: Option<&str>,
) -> QueueRequestResult<QueuedExecutionEnvelope> {
    let value = parse_json_rejecting_duplicate_keys(contents)?;
    let version = experimental_schema_version(&value)?;
    match version {
        LEGACY_QUEUED_EXECUTION_SCHEMA_VERSION..=QUEUED_EXECUTION_SCHEMA_VERSION => {
            reject_reserved_authority_keys(&value)?;
            Ok(serde_json::from_value(value)?)
        }
        5 => {
            validate_experimental_authority_v5(&value)?;
            if let Some(filename) = authoritative_filename {
                let job_id = value
                    .get("job_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_snapshot("queue request job_id must be a string"))?;
                if filename != format!("{job_id}.json") {
                    return Err(invalid_snapshot(
                        "authoritative queue filename disagrees with embedded job_id",
                    ));
                }
            }
            Err(QueueRequestError::ExperimentalAuthorityRefused)
        }
        version => Err(QueueRequestError::UnsupportedSchema { version }),
    }
}

#[cfg(feature = "experimental-authority-v5")]
const AUTHORITY_RESERVED_KEYS: &[&str] = &[
    "experimental_authority",
    "backend_policy",
    "authority_class",
    "output_disposition",
    "trust_proof",
    "protected_ref",
    "observed_protected_ref_sha",
];

#[cfg(feature = "experimental-authority-v5")]
const EXPERIMENTAL_AUTHORITY_REPOSITORY: &str = "Generous-Corp/pulp";

#[cfg(feature = "experimental-authority-v5")]
struct DuplicateRejectingJsonValue(Value);

#[cfg(feature = "experimental-authority-v5")]
impl<'de> Deserialize<'de> for DuplicateRejectingJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StrictValueVisitor;

        impl<'de> serde::de::Visitor<'de> for StrictValueVisitor {
            type Value = Value;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value without duplicate object keys")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(Value::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(Value::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(Value::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(Value::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(Value::String(value))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(Value::Null)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Value::Null)
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                DuplicateRejectingJsonValue::deserialize(deserializer).map(|value| value.0)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<DuplicateRejectingJsonValue>()? {
                    values.push(value.0);
                }
                Ok(Value::Array(values))
            }

            fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some(key) = object.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON object key {key:?}"
                        )));
                    }
                    let value = object.next_value::<DuplicateRejectingJsonValue>()?;
                    values.insert(key, value.0);
                }
                Ok(Value::Object(values))
            }
        }

        deserializer
            .deserialize_any(StrictValueVisitor)
            .map(DuplicateRejectingJsonValue)
    }
}

#[cfg(feature = "experimental-authority-v5")]
fn parse_json_rejecting_duplicate_keys(contents: &[u8]) -> QueueRequestResult<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(contents);
    let value = DuplicateRejectingJsonValue::deserialize(&mut deserializer)?.0;
    deserializer.end()?;
    Ok(value)
}

#[cfg(feature = "experimental-authority-v5")]
fn experimental_schema_version(value: &Value) -> QueueRequestResult<u32> {
    let Some(object) = value.as_object() else {
        return Err(invalid_snapshot("queued request must be a JSON object"));
    };
    let Some(version) = object.get("schema_version").and_then(Value::as_u64) else {
        return Err(invalid_snapshot(
            "queued request schema_version must be an unsigned integer",
        ));
    };
    u32::try_from(version).map_err(|_| QueueRequestError::UnsupportedSchema { version: u32::MAX })
}

#[cfg(feature = "experimental-authority-v5")]
fn reject_reserved_authority_keys(value: &Value) -> QueueRequestResult<()> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    reject_reserved_authority_keys_in_object(object)
}

#[cfg(feature = "experimental-authority-v5")]
fn reject_reserved_authority_keys_in_object(
    object: &serde_json::Map<String, Value>,
) -> QueueRequestResult<()> {
    for (key, value) in object {
        if AUTHORITY_RESERVED_KEYS.contains(&key.as_str()) {
            return Err(invalid_snapshot(format!(
                "authority-reserved key {key:?} is invalid before schema v5"
            )));
        }
        // These are user-defined maps in the pre-v5 schema. Their keys are
        // data, not envelope fields, so an authority-looking name is valid.
        if matches!(key.as_str(), "environment" | "stages") {
            continue;
        }
        match value {
            Value::Object(nested) => reject_reserved_authority_keys_in_object(nested)?,
            Value::Array(values) => {
                for value in values {
                    if let Value::Object(nested) = value {
                        reject_reserved_authority_keys_in_object(nested)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(feature = "experimental-authority-v5")]
fn validate_experimental_authority_v5(value: &Value) -> QueueRequestResult<()> {
    let envelope = exact_json_object(
        value,
        "queue request",
        &[
            "schema_version",
            "job_id",
            "kind",
            "cwd",
            "created_at",
            "execution_owner",
            "resource_plan",
            "request",
            "experimental_authority",
        ],
        &["provenance"],
    )?;
    require_u64(envelope, "schema_version", "queue request", 5)?;
    let job_id = require_string(envelope, "job_id", "queue request")?;
    validate_job_id(job_id)?;
    require_literal(envelope, "kind", "queue request", "run")?;
    require_string(envelope, "cwd", "queue request")?;
    let created_at = require_string(envelope, "created_at", "queue request")?;
    DateTime::parse_from_rfc3339(created_at)
        .map_err(|_| invalid_snapshot("queue request created_at must be RFC 3339"))?;
    require_one_of(
        envelope,
        "execution_owner",
        "queue request",
        &["legacy_unspecified", "foreground", "daemon"],
    )?;

    let provenance_head_sha = match envelope.get("provenance") {
        None | Some(Value::Null) => None,
        Some(value) => Some(validate_experimental_provenance(value)?),
    };
    validate_empty_experimental_resource_plan(required_value(
        envelope,
        "resource_plan",
        "queue request",
    )?)?;
    let request_sha = validate_empty_experimental_run_request(required_value(
        envelope,
        "request",
        "queue request",
    )?)?;
    validate_experimental_authority(
        required_value(envelope, "experimental_authority", "queue request")?,
        request_sha,
        provenance_head_sha,
    )
}

#[cfg(feature = "experimental-authority-v5")]
fn validate_experimental_provenance(value: &Value) -> QueueRequestResult<&str> {
    let provenance = exact_json_object(
        value,
        "provenance",
        &["canonical_cwd", "repo_root", "head_sha", "tree_signature"],
        &["repo_slug", "config_signature"],
    )?;
    require_string(provenance, "canonical_cwd", "provenance")?;
    require_string(provenance, "repo_root", "provenance")?;
    let head_sha = require_string(provenance, "head_sha", "provenance")?;
    require_exact_lower_sha1(head_sha, "provenance.head_sha")?;
    require_string(provenance, "tree_signature", "provenance")?;
    if let Some(repo_slug) = provenance.get("repo_slug") {
        let Some(repo_slug) = repo_slug.as_str() else {
            return Err(invalid_snapshot("provenance.repo_slug must be a string"));
        };
        if repo_slug != EXPERIMENTAL_AUTHORITY_REPOSITORY {
            return Err(invalid_snapshot(
                "provenance.repo_slug must match experimental_authority.trust_proof.repository",
            ));
        }
    }
    if let Some(config_signature) = provenance.get("config_signature")
        && !config_signature.is_string()
    {
        return Err(invalid_snapshot(
            "provenance.config_signature must be a string",
        ));
    }
    Ok(head_sha)
}

#[cfg(feature = "experimental-authority-v5")]
fn validate_empty_experimental_resource_plan(value: &Value) -> QueueRequestResult<()> {
    let resource_plan = exact_json_object(
        value,
        "resource_plan",
        &[
            "targets",
            "exclusive_claims",
            "cloud_targets",
            "host_pools",
            "vm_slots",
        ],
        &[],
    )?;
    for key in [
        "targets",
        "exclusive_claims",
        "cloud_targets",
        "host_pools",
        "vm_slots",
    ] {
        require_empty_array(resource_plan, key, "resource_plan")?;
    }
    Ok(())
}

#[cfg(feature = "experimental-authority-v5")]
fn validate_empty_experimental_run_request(value: &Value) -> QueueRequestResult<&str> {
    let request = exact_json_object(
        value,
        "request",
        &[
            "type",
            "branch",
            "sha",
            "mode",
            "priority",
            "warm_disabled",
            "fail_fast",
            "resume_from",
            "targets",
        ],
        &[],
    )?;
    require_literal(request, "type", "request", "run")?;
    if require_string(request, "branch", "request")?.is_empty() {
        return Err(invalid_snapshot("request.branch must not be empty"));
    }
    let sha = require_string(request, "sha", "request")?;
    require_exact_lower_sha1(sha, "request.sha")?;
    require_one_of(request, "mode", "request", &["full", "smoke"])?;
    require_one_of(request, "priority", "request", &["low", "normal", "high"])?;
    require_bool(request, "warm_disabled", "request")?;
    require_bool(request, "fail_fast", "request")?;
    match required_value(request, "resume_from", "request")? {
        Value::Null | Value::String(_) => {}
        _ => {
            return Err(invalid_snapshot(
                "request.resume_from must be a string or null",
            ));
        }
    }
    require_empty_array(request, "targets", "request")?;
    Ok(sha)
}

#[cfg(feature = "experimental-authority-v5")]
fn validate_experimental_authority(
    value: &Value,
    request_sha: &str,
    provenance_head_sha: Option<&str>,
) -> QueueRequestResult<()> {
    let authority = exact_json_object(
        value,
        "experimental_authority",
        &[
            "backend_policy",
            "authority_class",
            "output_disposition",
            "trust_proof",
        ],
        &[],
    )?;
    require_literal(
        authority,
        "backend_policy",
        "experimental_authority",
        "trusted_native_advisory",
    )?;
    require_literal(
        authority,
        "authority_class",
        "experimental_authority",
        "advisory",
    )?;
    require_literal(
        authority,
        "output_disposition",
        "experimental_authority",
        "quarantined_non_promotable",
    )?;
    let trust_proof = exact_json_object(
        required_value(authority, "trust_proof", "experimental_authority")?,
        "experimental_authority.trust_proof",
        &[
            "kind",
            "repository",
            "head_sha",
            "protected_ref",
            "observed_protected_ref_sha",
        ],
        &[],
    )?;
    require_literal(
        trust_proof,
        "kind",
        "experimental_authority.trust_proof",
        "protected_main_ancestor",
    )?;
    let repository = require_string(
        trust_proof,
        "repository",
        "experimental_authority.trust_proof",
    )?;
    if repository != EXPERIMENTAL_AUTHORITY_REPOSITORY || !is_valid_repository_slug(repository) {
        return Err(invalid_snapshot(
            "experimental_authority.trust_proof.repository must be exactly Generous-Corp/pulp",
        ));
    }
    let head_sha = require_string(
        trust_proof,
        "head_sha",
        "experimental_authority.trust_proof",
    )?;
    require_exact_lower_sha1(head_sha, "experimental_authority.trust_proof.head_sha")?;
    require_literal(
        trust_proof,
        "protected_ref",
        "experimental_authority.trust_proof",
        "refs/heads/main",
    )?;
    let observed_sha = require_string(
        trust_proof,
        "observed_protected_ref_sha",
        "experimental_authority.trust_proof",
    )?;
    require_exact_lower_sha1(
        observed_sha,
        "experimental_authority.trust_proof.observed_protected_ref_sha",
    )?;
    if head_sha != request_sha || provenance_head_sha.is_some_and(|sha| sha != request_sha) {
        return Err(invalid_snapshot(
            "experimental authority head SHA copies disagree",
        ));
    }
    Ok(())
}

#[cfg(feature = "experimental-authority-v5")]
fn exact_json_object<'a>(
    value: &'a Value,
    path: &str,
    required: &[&str],
    optional: &[&str],
) -> QueueRequestResult<&'a serde_json::Map<String, Value>> {
    let Some(object) = value.as_object() else {
        return Err(invalid_snapshot(format!("{path} must be an object")));
    };
    for key in object.keys() {
        if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
            return Err(invalid_snapshot(format!(
                "unknown or misplaced key {key:?} at {path}"
            )));
        }
    }
    for key in required {
        if !object.contains_key(*key) {
            return Err(invalid_snapshot(format!(
                "missing required key {key:?} at {path}"
            )));
        }
    }
    Ok(object)
}

#[cfg(feature = "experimental-authority-v5")]
fn required_value<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    path: &str,
) -> QueueRequestResult<&'a Value> {
    object
        .get(key)
        .ok_or_else(|| invalid_snapshot(format!("missing required key {key:?} at {path}")))
}

#[cfg(feature = "experimental-authority-v5")]
fn require_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    path: &str,
) -> QueueRequestResult<&'a str> {
    required_value(object, key, path)?
        .as_str()
        .ok_or_else(|| invalid_snapshot(format!("{path}.{key} must be a string")))
}

#[cfg(feature = "experimental-authority-v5")]
fn require_bool(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
) -> QueueRequestResult<bool> {
    required_value(object, key, path)?
        .as_bool()
        .ok_or_else(|| invalid_snapshot(format!("{path}.{key} must be a boolean")))
}

#[cfg(feature = "experimental-authority-v5")]
fn require_u64(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
    expected: u64,
) -> QueueRequestResult<()> {
    if required_value(object, key, path)?.as_u64() != Some(expected) {
        return Err(invalid_snapshot(format!(
            "{path}.{key} must be integer {expected}"
        )));
    }
    Ok(())
}

#[cfg(feature = "experimental-authority-v5")]
fn require_literal(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
    expected: &str,
) -> QueueRequestResult<()> {
    if require_string(object, key, path)? != expected {
        return Err(invalid_snapshot(format!(
            "{path}.{key} must be exactly {expected:?}"
        )));
    }
    Ok(())
}

#[cfg(feature = "experimental-authority-v5")]
fn require_one_of(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
    expected: &[&str],
) -> QueueRequestResult<()> {
    let value = require_string(object, key, path)?;
    if !expected.contains(&value) {
        return Err(invalid_snapshot(format!(
            "{path}.{key} has an unsupported literal"
        )));
    }
    Ok(())
}

#[cfg(feature = "experimental-authority-v5")]
fn require_empty_array(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
) -> QueueRequestResult<()> {
    if !matches!(required_value(object, key, path)?, Value::Array(values) if values.is_empty()) {
        return Err(invalid_snapshot(format!(
            "{path}.{key} must be an empty array"
        )));
    }
    Ok(())
}

#[cfg(feature = "experimental-authority-v5")]
fn require_exact_lower_sha1(value: &str, path: &str) -> QueueRequestResult<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_snapshot(format!(
            "{path} must be exactly 40 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn read_versioned_json<T>(path: &Path) -> QueueRequestResult<Option<T>>
where
    T: for<'de> Deserialize<'de>,
{
    read_versioned_json_through(path, QUEUED_EXECUTION_SCHEMA_VERSION)
}

fn read_versioned_json_through<T>(
    path: &Path,
    maximum_supported_version: u32,
) -> QueueRequestResult<Option<T>>
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
    if !(LEGACY_QUEUED_EXECUTION_SCHEMA_VERSION..=maximum_supported_version).contains(&version) {
        return Err(QueueRequestError::UnsupportedSchema { version });
    }
    Ok(Some(serde_json::from_value(value)?))
}

fn upgrade_legacy_request(
    mut envelope: QueuedExecutionEnvelope,
) -> QueueRequestResult<QueuedExecutionEnvelope> {
    if envelope.schema_version == QUEUED_EXECUTION_SCHEMA_VERSION {
        return Ok(envelope);
    }
    if !matches!(
        envelope.schema_version,
        LEGACY_QUEUED_EXECUTION_SCHEMA_VERSION
            | TRUSTED_ENVIRONMENT_QUEUED_EXECUTION_SCHEMA_VERSION
            | PREVIOUS_QUEUED_EXECUTION_SCHEMA_VERSION
    ) {
        return Err(QueueRequestError::UnsupportedSchema {
            version: envelope.schema_version,
        });
    }
    let targets = match &envelope.request {
        QueuedExecutionRequest::Run(request) => &request.targets,
        QueuedExecutionRequest::Ship(request) => &request.targets,
    };
    if envelope.schema_version == LEGACY_QUEUED_EXECUTION_SCHEMA_VERSION
        && targets.iter().any(target_has_trusted_environment)
    {
        return Err(invalid_snapshot(
            "legacy v1 request contains v2 trusted-environment fields",
        ));
    }
    if envelope.schema_version <= PREVIOUS_QUEUED_EXECUTION_SCHEMA_VERSION
        && targets.iter().any(target_has_integration_cleanup)
    {
        return Err(invalid_snapshot(
            "legacy request contains v4 integration checkout custody",
        ));
    }
    if let QueuedExecutionRequest::Ship(request) = &envelope.request
        && (request.metadata_authority_receipt.is_some() || request.targets.is_empty())
    {
        return Err(invalid_snapshot(
            "legacy request cannot carry or imply zero-target metadata authority",
        ));
    }
    envelope.schema_version = QUEUED_EXECUTION_SCHEMA_VERSION;
    Ok(envelope)
}

fn target_has_integration_cleanup(target: &QueuedResolvedTarget) -> bool {
    if matches!(
        &target.validation,
        QueuedValidationSnapshot::Local(validation) if validation.integration_cleanup.is_some()
    ) {
        return true;
    }
    match &target.backend {
        QueuedBackendSnapshot::HostPool(pool) => pool
            .members
            .iter()
            .any(|member| target_has_integration_cleanup(&member.target)),
        QueuedBackendSnapshot::Fallback(chain) => chain
            .backends
            .iter()
            .any(|backend| target_has_integration_cleanup(&backend.target)),
        _ => false,
    }
}

#[cfg(any(unix, test))]
fn validate_current_envelope(envelope: &QueuedExecutionEnvelope) -> QueueRequestResult<()> {
    validate_job_id(&envelope.job_id)?;
    match (&envelope.kind, &envelope.request) {
        (QueuedExecutionKind::Run, QueuedExecutionRequest::Run(request)) => {
            if request.branch.is_empty() || !is_exact_lower_hex_git_sha(&request.sha) {
                return Err(invalid_snapshot("queued run request has invalid identity"));
            }
        }
        (QueuedExecutionKind::Ship, QueuedExecutionRequest::Ship(request)) => {
            if request.pr == 0
                || !is_valid_repository_slug(&request.repo)
                || request.branch.is_empty()
                || request.base_branch.is_empty()
                || !is_exact_lower_hex_git_sha(&request.sha)
            {
                return Err(invalid_snapshot("queued ship request has invalid identity"));
            }
        }
        _ => {
            return Err(invalid_snapshot(
                "queued request kind disagrees with payload",
            ));
        }
    }
    Ok(())
}

fn validate_job_id(job_id: &str) -> QueueRequestResult<()> {
    if job_id.is_empty()
        || job_id.len() > 255
        || matches!(job_id, "." | "..")
        || job_id
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\'))
    {
        return Err(invalid_snapshot("queued record has an invalid job id"));
    }
    Ok(())
}

/// Apply the same schema and authority checks as the durable request reader.
#[cfg(any(unix, test))]
pub(crate) fn validate_queued_execution_envelope(
    envelope: QueuedExecutionEnvelope,
) -> QueueRequestResult<()> {
    let upgraded = upgrade_legacy_request(envelope)?;
    validate_current_envelope(&upgraded)
}

fn target_has_trusted_environment(target: &QueuedResolvedTarget) -> bool {
    if matches!(
        &target.validation,
        QueuedValidationSnapshot::Local(validation)
            if !validation.machine_environment.is_empty() || !validation.environment.is_empty()
    ) {
        return true;
    }
    match &target.backend {
        QueuedBackendSnapshot::HostPool(pool) => pool
            .members
            .iter()
            .any(|member| target_has_trusted_environment(&member.target)),
        QueuedBackendSnapshot::Fallback(chain) => chain
            .backends
            .iter()
            .any(|backend| target_has_trusted_environment(&backend.target)),
        _ => false,
    }
}

fn delete_if_present(path: &Path) -> QueueRequestResult<bool> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
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
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(dir)?;
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{
        ExecutionProvenance, HostPoolDemand, JobResourcePlan,
        MAX_SHIP_POST_VALIDATION_DETAIL_BYTES, QUEUED_EXECUTION_SCHEMA_VERSION, QueueOutcomeStore,
        QueueRequestError, QueueRequestStore, QueuedExecutionEnvelope, QueuedExecutionKind,
        QueuedExecutionOutcome, QueuedExecutionOwner, QueuedExecutionRequest,
        QueuedShipDisposition, QueuedShipDispositionKind, VmSlotDemand, parse_repo_slug,
        validate_queued_execution_envelope,
    };
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use crate::evidence::{evidence_resource_claim, run_evidence_scope, ship_evidence_scope};
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

    fn experimental_v5_request_value() -> Value {
        json!({
            "schema_version": 5,
            "job_id": "experimental-reader-only",
            "kind": "run",
            "cwd": "/work/pulp",
            "created_at": "2026-09-01T12:00:00Z",
            "execution_owner": "foreground",
            "provenance": {
                "canonical_cwd": "/work/pulp",
                "repo_root": "/work/pulp",
                "repo_slug": "Generous-Corp/pulp",
                "head_sha": "a".repeat(40),
                "tree_signature": "b".repeat(64),
                "config_signature": "c".repeat(64)
            },
            "resource_plan": {
                "targets": [],
                "exclusive_claims": [],
                "cloud_targets": [],
                "host_pools": [],
                "vm_slots": []
            },
            "request": {
                "type": "run",
                "branch": "main",
                "sha": "a".repeat(40),
                "mode": "full",
                "priority": "normal",
                "warm_disabled": false,
                "fail_fast": true,
                "resume_from": null,
                "targets": []
            },
            "experimental_authority": {
                "backend_policy": "trusted_native_advisory",
                "authority_class": "advisory",
                "output_disposition": "quarantined_non_promotable",
                "trust_proof": {
                    "kind": "protected_main_ancestor",
                    "repository": "Generous-Corp/pulp",
                    "head_sha": "a".repeat(40),
                    "protected_ref": "refs/heads/main",
                    "observed_protected_ref_sha": "d".repeat(40)
                }
            }
        })
    }

    fn request_store_with_contents(contents: &str) -> (tempfile::TempDir, QueueRequestStore) {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("request store");
        std::fs::write(store.path_for("experimental-reader-only"), contents)
            .expect("write request fixture");
        (temp, store)
    }

    fn request_store_with_value(value: &Value) -> (tempfile::TempDir, QueueRequestStore) {
        request_store_with_contents(&serde_json::to_string(value).expect("serialize fixture"))
    }

    const EXPERIMENTAL_AUTHORITY_FEATURE: &str = "experimental-authority-v5";
    const EXPERIMENTAL_AUTHORITY_CI_WORKFLOW: &str = ".github/workflows/ci.yml";
    const EXPERIMENTAL_AUTHORITY_CI_STEP: &str = r"      - name: Run experimental authority refusal tests
        if: runner.os == 'Linux'
        env:
          SHIPYARD_TEST_HOME: ${{ runner.temp }}
          RUST_MIN_STACK: 8388608
        run: cargo test --all-targets --locked --features ci-test-home,experimental-authority-v5";
    const REFUSAL_ONLY_GUIDANCE: [&str; 4] = [
        "docs/pulp-mac-cache-readiness.md",
        "docs/ship-state-machine.md",
        "skills/ci/SKILL.md",
        "skills/shipyard/SKILL.md",
    ];

    fn tracked_paths(repository: &Path) -> Vec<String> {
        let output = Command::new("git")
            .args(["-C", repository.to_str().expect("UTF-8 repository path")])
            .args(["ls-files", "-z"])
            .output()
            .expect("git ls-files must be available for the repository guard");
        assert!(
            output.status.success(),
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                String::from_utf8(path.to_vec()).expect("tracked paths must be valid UTF-8")
            })
            .collect()
    }

    fn is_official_invocation_surface(path: &str) -> bool {
        if path == ".shipyard/config.toml"
            || path == "install.sh"
            || path == "RELEASING.md"
            || path.starts_with(".github/workflows/")
            || path.starts_with(".githooks/")
            || path.starts_with("hooks/")
        {
            return true;
        }
        let file_name = path.rsplit('/').next().unwrap_or(path);
        let is_test_or_fixture = file_name.starts_with("test_")
            || path.contains("/tests/")
            || path.contains("/fixtures/");
        let is_script = [".sh", ".py", ".ps1", ".bash"]
            .iter()
            .any(|extension| path.ends_with(extension));
        !is_test_or_fixture && is_script
    }

    fn is_guidance_surface(path: &str) -> bool {
        path.starts_with(".claude-plugin/")
            || path.starts_with("agents/")
            || path.starts_with("commands/")
            || path.starts_with("hooks/")
            || path.starts_with("skills/")
            || path.starts_with("docs/")
    }

    fn contains_authority_vocabulary(contents: &str) -> bool {
        let contents = contents.to_ascii_lowercase();
        [
            EXPERIMENTAL_AUTHORITY_FEATURE,
            "experimental_authority",
            "trusted_native_advisory",
            "experimental authority schema v5",
            "schema-v5 experimental authority",
        ]
        .iter()
        .any(|needle| contents.contains(needle))
    }

    fn contains_activation(contents: &str) -> bool {
        let contents = contents.to_ascii_lowercase();
        contents.contains("--all-features")
            || contents.contains("--all_features")
            || contents.contains("all_features = true")
            || (contents.contains(EXPERIMENTAL_AUTHORITY_FEATURE)
                && (contents.contains("--features") || contents.contains("--features=")))
    }

    fn contains_affirmative_producer_instruction(contents: &str) -> bool {
        let mut previous = String::new();
        for line in contents.lines() {
            let line = line.to_ascii_lowercase();
            if !contains_authority_vocabulary(&line)
                && !line.contains("v5")
                && !line.contains("experimental authority")
            {
                previous = line;
                continue;
            }
            let continuation = previous
                .trim_end()
                .chars()
                .next_back()
                .is_some_and(|character| matches!(character, ',' | ':' | ';' | '\\'));
            let denial_context = if continuation {
                format!("{previous} {line}")
            } else {
                line.clone()
            };
            let denied = [
                "never",
                "must not",
                "cannot",
                "can't",
                "no writer",
                "no request",
                "no operational",
                "not an operational",
                "refus",
                "default-off",
                "compile-disabled",
                "only to test",
                "only validate",
            ]
            .iter()
            .any(|marker| denial_context.contains(marker));
            if denied {
                previous = line;
                continue;
            }
            if line
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
                .any(|word| {
                    matches!(
                        word,
                        "seed"
                            | "submit"
                            | "enqueue"
                            | "write"
                            | "create"
                            | "emit"
                            | "produce"
                            | "repair"
                            | "execute"
                            | "run"
                            | "enable"
                            | "deploy"
                    )
                })
            {
                return true;
            }
            previous = line;
        }
        false
    }

    fn default_reaches_feature(
        feature: &str,
        features: &toml::map::Map<String, toml::Value>,
        visiting: &mut BTreeSet<String>,
    ) -> bool {
        if feature == EXPERIMENTAL_AUTHORITY_FEATURE {
            return true;
        }
        if !visiting.insert(feature.to_owned()) {
            return false;
        }
        let reaches = features
            .get(feature)
            .and_then(toml::Value::as_array)
            .is_some_and(|members| {
                members
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .any(|member| {
                        !member.starts_with("dep:")
                            && !member.contains('/')
                            && default_reaches_feature(member, features, visiting)
                    })
            });
        visiting.remove(feature);
        reaches
    }

    fn assert_experimental_authority_guard_self_checks() {
        assert!(contains_activation("cargo test --all-features"));
        assert!(contains_activation(
            "cargo test --features ci-test-home,experimental-authority-v5"
        ));
        assert!(contains_activation(
            "cargo test --features\n  experimental-authority-v5"
        ));
        assert!(contains_affirmative_producer_instruction(
            "Submit a v5 record to enable experimental authority."
        ));
        assert!(!contains_affirmative_producer_instruction(
            "Never submit or execute a v5 record."
        ));
    }

    fn assert_ci_contains_only_source_checks(contents: &str) {
        assert_eq!(
            contents.matches(EXPERIMENTAL_AUTHORITY_FEATURE).count(),
            1,
            "experimental authority CI feature must have one exact test-only invocation"
        );
        assert!(
            contents.contains(EXPERIMENTAL_AUTHORITY_CI_STEP),
            "experimental authority CI invocation must retain its exact Linux test-only boundary"
        );
        assert!(
            !contents.contains("--all-features")
                && !contents.contains("--all_features")
                && !contents.contains("all_features = true"),
            "official CI must not enable every feature"
        );
    }

    fn assert_experimental_feature_is_dependency_free_and_default_off(repository: &Path) {
        let manifest_text =
            fs::read_to_string(repository.join("Cargo.toml")).expect("read repository Cargo.toml");
        let manifest: toml::Value = toml::from_str(&manifest_text).expect("parse Cargo.toml");
        let features = manifest
            .get("features")
            .and_then(toml::Value::as_table)
            .expect("Cargo.toml must contain a features table");
        assert!(
            features
                .get(EXPERIMENTAL_AUTHORITY_FEATURE)
                .and_then(toml::Value::as_array)
                .is_some_and(Vec::is_empty),
            "the experimental feature declaration must remain dependency-free"
        );
        for default_member in features
            .get("default")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .map(|member| {
                member
                    .as_str()
                    .expect("default feature members must be strings")
            })
        {
            assert!(
                !default_reaches_feature(default_member, features, &mut BTreeSet::new()),
                "Cargo default feature member {default_member:?} enables {EXPERIMENTAL_AUTHORITY_FEATURE}"
            );
        }
    }

    fn assert_experimental_authority_absent_from_surfaces(repository: &Path) {
        let refusal_only = REFUSAL_ONLY_GUIDANCE.into_iter().collect::<BTreeSet<_>>();
        for path in tracked_paths(repository) {
            if path == "Cargo.toml" || path == "src/queue_request.rs" {
                continue;
            }
            let full_path = repository.join(&path);
            let Ok(contents) = fs::read_to_string(&full_path) else {
                continue;
            };
            // Git may materialize checked-in workflow text with CRLF on
            // Windows. Keep the source-surface assertion byte-independent
            // while still requiring the exact normalized Linux-only block.
            let contents = contents.replace("\r\n", "\n");
            if is_official_invocation_surface(&path) {
                if path == EXPERIMENTAL_AUTHORITY_CI_WORKFLOW {
                    assert_ci_contains_only_source_checks(&contents);
                } else {
                    assert!(
                        !contents.contains(EXPERIMENTAL_AUTHORITY_FEATURE),
                        "official invocation surface {path} mentions or enables {EXPERIMENTAL_AUTHORITY_FEATURE}"
                    );
                    assert!(
                        !contains_activation(&contents),
                        "official invocation surface {path} enables all features"
                    );
                }
            }
            if !is_guidance_surface(&path) || !contains_authority_vocabulary(&contents) {
                continue;
            }
            assert!(
                refusal_only.contains(path.as_str()),
                "producer guidance {path} introduces experimental authority vocabulary"
            );
            let lower = contents.to_ascii_lowercase();
            assert!(
                lower.contains("refus")
                    && (lower.contains("v4-only")
                        || lower.contains("compile-disabled")
                        || lower.contains("official build")),
                "allowed guidance {path} must remain explicitly refusal-only"
            );
            assert!(
                !contains_activation(&contents),
                "allowed refusal-only guidance {path} contains an activation command"
            );
            assert!(
                !contains_affirmative_producer_instruction(&contents),
                "allowed refusal-only guidance {path} contains producer instructions"
            );
        }
    }

    #[test]
    fn experimental_authority_feature_is_absent_from_official_and_producer_surfaces() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_experimental_authority_guard_self_checks();
        assert_experimental_feature_is_dependency_free_and_default_off(repository);
        assert_experimental_authority_absent_from_surfaces(repository);
    }

    #[cfg(not(feature = "experimental-authority-v5"))]
    #[test]
    fn default_reader_keeps_v5_unsupported() {
        let (_temp, store) = request_store_with_value(&experimental_v5_request_value());

        let error = store
            .load("experimental-reader-only")
            .expect_err("v5 rejected");

        assert!(matches!(
            error,
            QueueRequestError::UnsupportedSchema { version: 5 }
        ));
    }

    #[cfg(feature = "experimental-authority-v5")]
    fn assert_experimental_v5_invalid(value: &Value) {
        let (_temp, store) = request_store_with_value(value);
        let error = store
            .load("experimental-reader-only")
            .expect_err("invalid v5 must fail closed");
        assert!(
            !matches!(error, QueueRequestError::ExperimentalAuthorityRefused),
            "invalid fixture reached typed refusal: {value}"
        );
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn valid_experimental_v5_is_refused_without_returning_an_envelope() {
        let encoded = serde_json::to_vec(&experimental_v5_request_value()).expect("fixture");
        assert!(matches!(
            super::decode_queued_execution_request_bytes(&encoded),
            Err(QueueRequestError::ExperimentalAuthorityRefused)
        ));

        let (_temp, store) = request_store_with_value(&experimental_v5_request_value());

        assert!(matches!(
            store.load("experimental-reader-only"),
            Err(QueueRequestError::ExperimentalAuthorityRefused)
        ));
        assert!(matches!(
            store.list(),
            Err(QueueRequestError::ExperimentalAuthorityRefused)
        ));
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_v5_rejects_filename_job_id_mismatch_on_store_reads() {
        let value = experimental_v5_request_value();
        let contents = serde_json::to_string(&value).expect("serialize fixture");
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("request store");
        std::fs::write(store.path().join("different-job.json"), contents)
            .expect("write mismatched request fixture");

        let error = store
            .list()
            .expect_err("mismatched filename must fail closed");
        assert!(matches!(error, QueueRequestError::InvalidSnapshot { .. }));
        assert!(error.to_string().contains("filename disagrees"));
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_v5_rejects_duplicate_keys_at_every_object_depth() {
        let encoded = serde_json::to_string(&experimental_v5_request_value()).expect("fixture");
        let duplicates = [
            encoded.replacen('{', "{\"schema_version\":5,", 1),
            encoded.replacen(
                "\"provenance\":{",
                "\"provenance\":{\"head_sha\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
                1,
            ),
            encoded.replacen(
                "\"resource_plan\":{",
                "\"resource_plan\":{\"targets\":[],",
                1,
            ),
            encoded.replacen("\"request\":{", "\"request\":{\"type\":\"run\",", 1),
            encoded.replacen(
                "\"experimental_authority\":{",
                "\"experimental_authority\":{\"backend_policy\":\"trusted_native_advisory\",",
                1,
            ),
            encoded.replacen(
                "\"trust_proof\":{",
                "\"trust_proof\":{\"kind\":\"protected_main_ancestor\",",
                1,
            ),
        ];
        for duplicate in duplicates {
            let (_temp, store) = request_store_with_contents(&duplicate);
            assert!(matches!(
                store.load("experimental-reader-only"),
                Err(QueueRequestError::Json(_))
            ));
        }
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_v5_rejects_unknown_missing_and_misplaced_keys() {
        let fixture = experimental_v5_request_value();
        let mut cases = Vec::new();
        for path in [
            &[][..],
            &["provenance"][..],
            &["resource_plan"][..],
            &["request"][..],
            &["experimental_authority"][..],
            &["experimental_authority", "trust_proof"][..],
        ] {
            let mut value = fixture.clone();
            let mut object = &mut value;
            for component in path {
                object = &mut object[*component];
            }
            object
                .as_object_mut()
                .expect("fixture object")
                .insert("unknown_field".to_owned(), json!(true));
            cases.push(value);
        }
        let mut missing = fixture.clone();
        missing["request"]
            .as_object_mut()
            .expect("request")
            .remove("targets");
        cases.push(missing);
        let mut misplaced = fixture.clone();
        misplaced["request"]["trust_proof"] = json!({});
        cases.push(misplaced);

        for value in cases {
            assert_experimental_v5_invalid(&value);
        }
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_reader_rejects_reserved_authority_keys_in_v1_through_v4() {
        for version in 1..=4 {
            for reserved in super::AUTHORITY_RESERVED_KEYS {
                let mut value = experimental_v5_request_value();
                value["schema_version"] = json!(version);
                value
                    .as_object_mut()
                    .expect("envelope")
                    .remove("experimental_authority");
                value["request"][*reserved] = json!("misplaced");
                let (_temp, store) = request_store_with_value(&value);
                assert!(matches!(
                    store.load("experimental-reader-only"),
                    Err(QueueRequestError::InvalidSnapshot { .. })
                ));
            }
        }
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_reader_preserves_reserved_names_in_user_defined_v4_maps() {
        let envelope = QueuedExecutionEnvelope::from_run_request(
            "job-v4-map-keys",
            "/work/repo",
            &run_request(),
        );
        let mut value = serde_json::to_value(envelope).expect("serialize v4 envelope");
        value["request"]["targets"][0]["validation"]["environment"]["trust_proof"] =
            json!("user-defined");
        value["request"]["targets"][0]["validation"]["stages"]["backend_policy"] =
            json!("cargo test");

        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("request store");
        std::fs::write(
            store.path_for("job-v4-map-keys"),
            serde_json::to_vec(&value).expect("serialize mutated envelope"),
        )
        .expect("write v4 fixture");

        store
            .load("job-v4-map-keys")
            .expect("user-defined map keys remain valid")
            .expect("v4 envelope is present");
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_v5_rejects_wrong_types_case_and_exact_identity_drift() {
        let fixture = experimental_v5_request_value();
        let mut cases = Vec::new();

        let mut wrong_type = fixture.clone();
        wrong_type["request"]["warm_disabled"] = json!("false");
        cases.push(wrong_type);
        let mut wrong_policy_case = fixture.clone();
        wrong_policy_case["experimental_authority"]["authority_class"] = json!("Advisory");
        cases.push(wrong_policy_case);
        let mut wrong_repository_case = fixture.clone();
        wrong_repository_case["experimental_authority"]["trust_proof"]["repository"] =
            json!("generous-corp/pulp");
        cases.push(wrong_repository_case);
        let mut invalid_repository = fixture.clone();
        invalid_repository["experimental_authority"]["trust_proof"]["repository"] =
            json!("Generous-Corp/pulp/extra");
        cases.push(invalid_repository);
        let mut uppercase_sha = fixture.clone();
        uppercase_sha["experimental_authority"]["trust_proof"]["head_sha"] = json!("A".repeat(40));
        cases.push(uppercase_sha);
        let mut long_sha = fixture.clone();
        long_sha["experimental_authority"]["trust_proof"]["observed_protected_ref_sha"] =
            json!("d".repeat(64));
        cases.push(long_sha);
        let mut wrong_ref = fixture.clone();
        wrong_ref["experimental_authority"]["trust_proof"]["protected_ref"] =
            json!("refs/heads/trunk");
        cases.push(wrong_ref);
        let mut cross_field = fixture.clone();
        cross_field["experimental_authority"]["trust_proof"]["head_sha"] = json!("e".repeat(40));
        cases.push(cross_field);
        let mut provenance_cross_field = fixture.clone();
        provenance_cross_field["provenance"]["head_sha"] = json!("e".repeat(40));
        cases.push(provenance_cross_field);

        for value in cases {
            assert_experimental_v5_invalid(&value);
        }
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_v5_rejects_provenance_repository_different_from_frozen_trust_repository() {
        let mut value = experimental_v5_request_value();
        value["provenance"]["repo_slug"] = json!("Generous-Corp/other");
        let (_temp, store) = request_store_with_value(&value);

        let error = store
            .load("experimental-reader-only")
            .expect_err("mismatched provenance repository must fail closed");
        assert!(matches!(error, QueueRequestError::InvalidSnapshot { .. }));
        assert!(
            error
                .to_string()
                .contains("provenance.repo_slug must match")
        );
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_v5_preserves_optional_provenance_and_repo_slug() {
        let mut without_provenance = experimental_v5_request_value();
        without_provenance
            .as_object_mut()
            .expect("request envelope")
            .remove("provenance");
        let mut without_repo_slug = experimental_v5_request_value();
        without_repo_slug["provenance"]
            .as_object_mut()
            .expect("provenance")
            .remove("repo_slug");
        let mut null_provenance = experimental_v5_request_value();
        null_provenance["provenance"] = Value::Null;

        for value in [without_provenance, without_repo_slug, null_provenance] {
            let (_temp, store) = request_store_with_value(&value);
            assert!(matches!(
                store.load("experimental-reader-only"),
                Err(QueueRequestError::ExperimentalAuthorityRefused)
            ));
        }
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_v5_rejects_nonempty_resource_and_execution_collections() {
        let fixture = experimental_v5_request_value();
        for key in [
            "targets",
            "exclusive_claims",
            "cloud_targets",
            "host_pools",
            "vm_slots",
        ] {
            let mut value = fixture.clone();
            value["resource_plan"][key] = json!(["occupied"]);
            assert_experimental_v5_invalid(&value);
        }
        let mut request_target = fixture;
        request_target["request"]["targets"] = json!([{}]);
        assert_experimental_v5_invalid(&request_target);
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_v5_rejects_ship_and_nonadvisory_policy_shapes() {
        let fixture = experimental_v5_request_value();
        let mut cases = Vec::new();
        let mut ship_kind = fixture.clone();
        ship_kind["kind"] = json!("ship");
        cases.push(ship_kind);
        let mut ship_request = fixture.clone();
        ship_request["request"]["type"] = json!("ship");
        cases.push(ship_request);
        let mut tart_required = fixture.clone();
        tart_required["experimental_authority"]["backend_policy"] = json!("tart_required");
        cases.push(tart_required);
        let mut promotable = fixture;
        promotable["experimental_authority"]["output_disposition"] = json!("promotable");
        cases.push(promotable);

        for value in cases {
            assert_experimental_v5_invalid(&value);
        }
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_v5_rejects_trailing_json_and_every_outcome_shape() {
        let encoded = serde_json::to_string(&experimental_v5_request_value()).expect("fixture");
        let (_temp, store) = request_store_with_contents(&format!("{encoded} true"));
        assert!(matches!(
            store.load("experimental-reader-only"),
            Err(QueueRequestError::Json(_))
        ));

        for outcome in [
            json!({"type": "run", "schema_version": 5, "job_id": "future-run"}),
            json!({
                "type": "ship",
                "schema_version": 5,
                "job_id": "future-ship",
                "experimental_authority": {"output_disposition": "quarantined_non_promotable"}
            }),
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let store = QueueOutcomeStore::new(temp.path()).expect("outcome store");
            std::fs::write(
                store.path_for(outcome["job_id"].as_str().expect("job id")),
                serde_json::to_vec(&outcome).expect("outcome fixture"),
            )
            .expect("write outcome");
            assert!(matches!(
                store.load(outcome["job_id"].as_str().expect("job id")),
                Err(QueueRequestError::UnsupportedSchema { version: 5 })
            ));
        }
    }

    #[cfg(feature = "experimental-authority-v5")]
    #[test]
    fn experimental_feature_does_not_raise_any_writer_or_constructor_ceiling() {
        let temp = tempfile::tempdir().expect("tempdir");
        let request_store = QueueRequestStore::new(temp.path()).expect("request store");
        let outcome_store = QueueOutcomeStore::new(temp.path()).expect("outcome store");
        let mut request = run_request();
        request.targets.clear();
        let mut envelope =
            QueuedExecutionEnvelope::from_run_request("writer-ceiling", "/work/repo", &request);
        let outcome = QueuedExecutionOutcome::run("writer-ceiling");
        assert_eq!(envelope.schema_version, QUEUED_EXECUTION_SCHEMA_VERSION);
        assert_eq!(outcome.schema_version(), QUEUED_EXECUTION_SCHEMA_VERSION);

        envelope.schema_version = 5;
        assert!(matches!(
            request_store.save(&envelope),
            Err(QueueRequestError::UnsupportedSchema { version: 5 })
        ));
        let future_outcome = QueuedExecutionOutcome::Run {
            schema_version: 5,
            job_id: "writer-ceiling".to_owned(),
        };
        assert!(matches!(
            outcome_store.save(&future_outcome),
            Err(QueueRequestError::UnsupportedSchema { version: 5 })
        ));
    }

    #[test]
    fn repository_slug_accepts_only_canonical_authenticated_github_origins() {
        assert_eq!(
            parse_repo_slug("git@github.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_repo_slug("https://github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        for hostile in [
            "http://github.com/owner/repo.git",
            "https://github.com.evil.test/owner/repo.git",
            "https://user@github.com/owner/repo.git",
        ] {
            assert_eq!(
                parse_repo_slug(hostile),
                None,
                "accepted hostile origin {hostile}"
            );
        }
    }

    #[test]
    fn unattended_provenance_rejects_resolved_config_drift() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let global = temp.path().join("global");
        std::fs::create_dir_all(&repo).expect("repo");
        std::fs::create_dir_all(&global).expect("global");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Shipyard Test"],
            vec!["config", "user.email", "shipyard@example.invalid"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .status()
                    .expect("git")
                    .success()
            );
        }
        std::fs::write(repo.join("tracked.txt"), "stable\n").expect("tracked file");
        assert!(
            std::process::Command::new("git")
                .args(["add", "tracked.txt"])
                .current_dir(&repo)
                .status()
                .expect("git add")
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["commit", "-qm", "fixture"])
                .current_dir(&repo)
                .status()
                .expect("git commit")
                .success()
        );
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .expect("head");
        let head = String::from_utf8(head.stdout)
            .expect("utf8")
            .trim()
            .to_owned();
        std::fs::write(global.join("config.toml"), "[queue]\nmax_workers = 2\n").expect("config");
        let original =
            LoadedConfig::load(Some(global.clone()), None, None, LocalOverlaySource::None)
                .expect("original config");
        let provenance = ExecutionProvenance::capture_with_config(&repo, None, &head, &original)
            .expect("provenance");
        provenance
            .validate_with_config(&repo, &original)
            .expect("unchanged config");

        std::fs::write(
            global.join("config.toml"),
            "[queue]\nmax_workers = 2\n[repository_environment.\"Generous-Corp/forge\"]\nPULP_SDK_DIR = \"/new/sdk\"\n",
        )
        .expect("repository environment drift");
        let repository_environment_changed =
            LoadedConfig::load(Some(global.clone()), None, None, LocalOverlaySource::None)
                .expect("changed repository environment");
        provenance
            .validate_with_config(&repo, &repository_environment_changed)
            .expect("queued snapshot owns resolved repository environment");

        std::fs::write(global.join("config.toml"), "[queue]\nmax_workers = 3\n")
            .expect("config drift");
        let changed = LoadedConfig::load(Some(global), None, None, LocalOverlaySource::None)
            .expect("changed config");
        let error = provenance
            .validate_with_config(&repo, &changed)
            .expect_err("config drift must fail closed");
        assert!(error.to_string().contains("configuration changed"));
    }

    fn local_target() -> ResolvedTarget {
        local_target_with_name("mac", Some(PathBuf::from("/repo")))
    }

    fn integration_snapshot(
        source_repo: &Path,
        checkout_parent: &Path,
    ) -> crate::changed_surface::integration_checkout::IntegrationCheckoutSnapshot {
        serde_json::from_value(json!({
            "source_repo": source_repo,
            "checkout_parent": checkout_parent,
            "receipt": {
                "schema_version": 1,
                "disposition": "recomputed",
                "merge_authority": "blocked_until_current_merge_tree",
                "repository": "owner/repo",
                "pull_request": 7,
                "target": "mac",
                "head_sha": "a".repeat(40),
                "head_tree_sha": "b".repeat(40),
                "old_protected_base_sha": "c".repeat(40),
                "live_protected_base_sha": "d".repeat(40),
                "merge_base_sha": "c".repeat(40),
                "integration_tree_sha": "e".repeat(40),
                "integration_commit_sha": "f".repeat(40),
                "changed_paths_digest": "1".repeat(64),
                "protected_base_delta_digest": "2".repeat(64),
                "old_policy_digest": "3".repeat(64),
                "live_policy_digest": "3".repeat(64),
                "old_workflow_digest": "4".repeat(64),
                "live_workflow_digest": "4".repeat(64),
                "validation_contract_digest": "5".repeat(64),
                "integration_changed_paths_digest": "6".repeat(64),
                "reason": "bounded_shadow_recomputed"
            }
        }))
        .expect("integration snapshot")
    }

    fn local_target_with_name(name: &str, cwd: Option<PathBuf>) -> ResolvedTarget {
        let mut stages = BTreeMap::new();
        stages.insert("test".to_owned(), "cargo test".to_owned());
        ResolvedTarget {
            name: name.to_owned(),
            validation_build_type: None,
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
                machine_environment: vec!["PULP_SDK_DIR".to_owned()],
                environment: BTreeMap::from([(
                    "PULP_SDK_DIR".to_owned(),
                    "/machine/pulp-sdk".to_owned(),
                )]),
                integration_cleanup: None,
            }),
            failure_parser: Some("auto".to_owned()),
        }
    }

    fn ssh_target(name: &str, host: &str, repo_path: &str) -> ResolvedTarget {
        ResolvedTarget {
            name: name.to_owned(),
            validation_build_type: None,
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
            validation_build_type: None,
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
        cloud_target_with_platform(name, "linux")
    }

    fn cloud_target_with_platform(name: &str, platform: &str) -> ResolvedTarget {
        ResolvedTarget {
            name: name.to_owned(),
            validation_build_type: None,
            platform: platform.to_owned(),
            backend_name: "cloud".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: ResolvedBackend::Cloud(CloudTargetConfig {
                name: name.to_owned(),
                platform: platform.to_owned(),
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
            sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
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
            sha: "dddddddddddddddddddddddddddddddddddddddd".to_owned(),
            commit_subject: "queue request store".to_owned(),
            pr_url: Some("https://github.com/danielraffel/shipyard/pull/42".to_owned()),
            pr_title: Some("Queue request store".to_owned()),
            mode: ValidationMode::Full,
            priority: Priority::High,
            warm_disabled: true,
            fail_fast: false,
            resume_from: None,
            advisory_targets: BTreeSet::from(["mac".to_owned()]),
            adopt_head: false,
            pr_snapshot_file: None,
            metadata_authority_receipt: None,
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
    fn request_mutation_lock_excludes_concurrent_store_writer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let envelope =
            QueuedExecutionEnvelope::from_run_request("job-locked", temp.path(), &run_request());
        let guard = store
            .acquire_mutation_lock(&envelope.job_id)
            .expect("exclusive request lock");
        let writer = store.clone();
        let submitted = envelope.clone();
        let (sent, received) = mpsc::channel();
        let thread = thread::spawn(move || {
            let result = writer.save(&submitted);
            sent.send(result).expect("send result");
        });

        assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
        drop(guard);
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("writer unblocked")
            .expect("save");
        thread.join().expect("writer thread");
    }

    #[test]
    fn metadata_authority_ship_round_trips_without_worker_capacity() {
        let mut request = ship_request();
        request.targets.clear();
        request.metadata_authority_receipt =
            Some(crate::metadata_authority::MetadataAuthorityReceipt {
                schema_version: 1,
                repository: "danielraffel/shipyard".to_owned(),
                pull_request: 42,
                base_ref: "main".to_owned(),
                base_sha: "a".repeat(40),
                head_sha: request.sha.clone(),
                tree_sha: "b".repeat(40),
                observation_target: "mac".to_owned(),
                policy_digest: "c".repeat(64),
                changed_paths_digest: "d".repeat(64),
                required_checks_digest: "e".repeat(64),
                changed_paths: vec!["docs/guide.md".to_owned()],
                required_checks: vec!["docs".to_owned()],
                hosted_checks: vec![crate::metadata_authority::HostedCheckObservation {
                    name: "docs".to_owned(),
                    status: "COMPLETED".to_owned(),
                    conclusion: "SUCCESS".to_owned(),
                    producer: "app:15368".to_owned(),
                }],
            });
        let envelope =
            QueuedExecutionEnvelope::from_ship_request("metadata", "/work/repo", &request);
        assert!(envelope.resource_plan.targets.is_empty());
        assert!(envelope.resource_plan.cloud_targets.is_empty());
        assert!(envelope.resource_plan.host_pools.is_empty());
        assert!(envelope.resource_plan.vm_slots.is_empty());
        assert_eq!(
            envelope.to_ship_request().expect("restore metadata ship"),
            request
        );

        let mut downgraded = serde_json::to_value(&envelope).expect("serialize metadata request");
        downgraded["schema_version"] = json!(2);
        let decoded: QueuedExecutionEnvelope =
            serde_json::from_value(downgraded).expect("decode downgraded metadata request");
        assert!(super::upgrade_legacy_request(decoded).is_err());
    }

    #[test]
    fn current_reader_safely_upgrades_a_legacy_v1_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let envelope =
            QueuedExecutionEnvelope::from_run_request("job-v1", "/work/repo", &run_request());
        let mut value = serde_json::to_value(envelope).expect("serialize request");
        value["schema_version"] = json!(1);
        let validation = value
            .pointer_mut("/request/targets/0/validation")
            .and_then(Value::as_object_mut)
            .expect("local validation snapshot");
        validation.remove("machine_environment");
        validation.remove("environment");
        std::fs::write(
            store.path_for("job-v1"),
            serde_json::to_vec_pretty(&value).expect("encode v1"),
        )
        .expect("write v1");

        let loaded = store.load("job-v1").expect("load v1").expect("present");

        assert_eq!(loaded.schema_version, QUEUED_EXECUTION_SCHEMA_VERSION);
        let request = loaded.to_run_request().expect("restore v1");
        let ResolvedValidation::Local(validation) = &request.targets[0].validation else {
            panic!("expected local validation");
        };
        assert!(validation.machine_environment.is_empty());
        assert!(validation.environment.is_empty());
    }

    #[test]
    fn downgraded_v1_request_cannot_smuggle_v2_trusted_environment() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let envelope = QueuedExecutionEnvelope::from_run_request(
            "job-downgraded",
            "/work/repo",
            &run_request(),
        );
        let mut value = serde_json::to_value(envelope).expect("serialize request");
        value["schema_version"] = json!(1);
        std::fs::write(
            store.path_for("job-downgraded"),
            serde_json::to_vec_pretty(&value).expect("encode downgraded v1"),
        )
        .expect("write downgraded v1");

        let error = store
            .load("job-downgraded")
            .expect_err("v1 must not carry v2 trusted environment");

        assert!(matches!(error, QueueRequestError::InvalidSnapshot { .. }));
        assert!(error.to_string().contains("v2 trusted-environment"));
    }

    #[test]
    fn legacy_v3_reader_rejects_current_v4_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let envelope =
            QueuedExecutionEnvelope::from_run_request("job-v4", "/work/repo", &run_request());
        store.save(&envelope).expect("save v4");

        let error = super::read_versioned_json_through::<QueuedExecutionEnvelope>(
            &store.path_for("job-v4"),
            3,
        )
        .expect_err("v3 reader must reject v4");

        assert!(matches!(
            error,
            QueueRequestError::UnsupportedSchema { version: 4 }
        ));
    }

    #[test]
    fn current_reader_upgrades_an_ordinary_v3_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let envelope =
            QueuedExecutionEnvelope::from_run_request("job-v3", "/work/repo", &run_request());
        let mut value = serde_json::to_value(envelope).expect("serialize request");
        value["schema_version"] = json!(3);
        std::fs::write(
            store.path_for("job-v3"),
            serde_json::to_vec_pretty(&value).expect("encode v3"),
        )
        .expect("write v3");

        let loaded = store.load("job-v3").expect("load v3").expect("present");
        assert_eq!(loaded.schema_version, QUEUED_EXECUTION_SCHEMA_VERSION);
        loaded.to_run_request().expect("restore v3");
    }

    #[test]
    fn downgraded_v3_request_cannot_smuggle_v4_checkout_custody() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(temp.path())
                .status()
                .expect("git init")
                .success()
        );
        let mut envelope = QueuedExecutionEnvelope::from_run_request(
            "job-v3-custody",
            temp.path(),
            &run_request(),
        );
        envelope.schema_version = 3;
        let QueuedExecutionRequest::Run(request) = &mut envelope.request else {
            panic!("run request");
        };
        let super::QueuedValidationSnapshot::Local(validation) = &mut request.targets[0].validation
        else {
            panic!("local validation");
        };
        validation.integration_cleanup = Some(Box::new(integration_snapshot(
            temp.path(),
            &temp.path().join("state/integration-checkouts"),
        )));

        let error = super::upgrade_legacy_request(envelope)
            .expect_err("v3 must not carry v4 checkout custody");
        assert!(matches!(error, QueueRequestError::InvalidSnapshot { .. }));
        assert!(
            error
                .to_string()
                .contains("v4 integration checkout custody")
        );
    }

    #[test]
    fn downgraded_v3_request_cannot_smuggle_nested_v4_checkout_custody() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut nested = super::QueuedResolvedTarget::from(&local_target());
        let super::QueuedValidationSnapshot::Local(validation) = &mut nested.validation else {
            panic!("local validation");
        };
        validation.integration_cleanup = Some(Box::new(integration_snapshot(
            temp.path(),
            &temp.path().join("state/integration-checkouts"),
        )));
        let host_pool = super::QueuedResolvedTarget {
            name: "pool".to_owned(),
            validation_build_type: None,
            platform: "macos".to_owned(),
            backend_name: "host_pool".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: super::QueuedBackendSnapshot::HostPool(super::QueuedHostPoolTarget {
                pool_name: "pool".to_owned(),
                strategy: "ordered".to_owned(),
                lease_stale_seconds: 60,
                heartbeat_interval_seconds: 10,
                requires: Vec::new(),
                members: vec![super::QueuedHostPoolMember {
                    id: "member".to_owned(),
                    target: Box::new(nested.clone()),
                    label: "member".to_owned(),
                    profile_label: "member".to_owned(),
                    max_concurrency: 1,
                    capabilities: Vec::new(),
                }],
            }),
            validation: super::QueuedValidationSnapshot::HostPool,
            failure_parser: None,
        };
        let fallback = super::QueuedResolvedTarget {
            name: "fallback".to_owned(),
            validation_build_type: None,
            platform: "macos".to_owned(),
            backend_name: "fallback".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: super::QueuedBackendSnapshot::Fallback(super::QueuedFallbackTarget {
                backends: vec![super::QueuedFallbackBackend {
                    target: Box::new(nested),
                    label: "local".to_owned(),
                    profile_label: "local".to_owned(),
                    capabilities: Vec::new(),
                }],
                requires: Vec::new(),
                heartbeat_stale_secs: 60,
            }),
            validation: super::QueuedValidationSnapshot::Fallback,
            failure_parser: None,
        };

        for (suffix, target) in [("host-pool", host_pool), ("fallback", fallback)] {
            let mut envelope = QueuedExecutionEnvelope::from_run_request(
                format!("job-v3-{suffix}"),
                temp.path(),
                &run_request(),
            );
            envelope.schema_version = 3;
            let QueuedExecutionRequest::Run(request) = &mut envelope.request else {
                panic!("run request");
            };
            request.targets = vec![target];
            let error = super::upgrade_legacy_request(envelope)
                .expect_err("nested v4 custody must not survive v3 upgrade");
            assert!(
                error
                    .to_string()
                    .contains("v4 integration checkout custody")
            );
        }
    }

    #[test]
    fn restored_checkout_custody_must_equal_local_execution_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(temp.path())
                .status()
                .expect("git init")
                .success()
        );
        std::fs::create_dir_all(temp.path().join("state")).expect("state root");
        let snapshot = integration_snapshot(
            temp.path(),
            &temp.path().join("state/integration-checkouts"),
        );
        let expected_cwd = snapshot.restore().expect("restore snapshot").path;
        let mut envelope = QueuedExecutionEnvelope::from_run_request(
            "job-v4-custody",
            temp.path(),
            &run_request(),
        );
        {
            let QueuedExecutionRequest::Run(request) = &mut envelope.request else {
                panic!("run request");
            };
            let super::QueuedValidationSnapshot::Local(validation) =
                &mut request.targets[0].validation
            else {
                panic!("local validation");
            };
            validation.integration_cleanup = Some(Box::new(snapshot.clone()));
            let super::QueuedBackendSnapshot::Local(backend) = &mut request.targets[0].backend
            else {
                panic!("local backend");
            };
            backend.cwd = Some(temp.path().join("wrong-checkout"));
        }
        let error = envelope
            .to_run_request()
            .expect_err("mismatched execution cwd must refuse");
        assert!(
            error
                .to_string()
                .contains("does not match local execution cwd")
        );

        let QueuedExecutionRequest::Run(request) = &mut envelope.request else {
            panic!("run request");
        };
        let super::QueuedBackendSnapshot::Local(backend) = &mut request.targets[0].backend else {
            panic!("local backend");
        };
        backend.cwd = Some(expected_cwd);
        envelope
            .to_run_request()
            .expect("matching exact checkout custody");
    }

    #[test]
    fn current_reader_upgrades_an_ordinary_v2_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let envelope =
            QueuedExecutionEnvelope::from_run_request("job-v2", "/work/repo", &run_request());
        let mut value = serde_json::to_value(envelope).expect("serialize request");
        value["schema_version"] = json!(2);
        std::fs::write(
            store.path_for("job-v2"),
            serde_json::to_vec_pretty(&value).expect("encode v2"),
        )
        .expect("write v2");

        let loaded = store.load("job-v2").expect("load v2").expect("present");
        assert_eq!(loaded.schema_version, QUEUED_EXECUTION_SCHEMA_VERSION);
    }

    #[cfg(unix)]
    #[test]
    fn request_store_keeps_snapshots_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let envelope =
            QueuedExecutionEnvelope::from_run_request("job-private", "/work/repo", &run_request());

        store.save_durable(&envelope).expect("save durable");

        let directory_mode = std::fs::metadata(store.path())
            .expect("request directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let request_mode = std::fs::metadata(store.path_for("job-private"))
            .expect("request metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(request_mode, 0o600);
    }

    #[test]
    fn daemon_admission_requires_signed_configuration_provenance() {
        let mut envelope =
            QueuedExecutionEnvelope::from_run_request("job-run", "/work/repo", &run_request());
        envelope.execution_owner = QueuedExecutionOwner::Daemon;
        envelope.provenance = None;

        assert!(envelope.is_daemon_owned());
        assert!(!envelope.is_daemon_admissible());

        envelope.provenance = Some(ExecutionProvenance {
            canonical_cwd: PathBuf::from("/work/repo"),
            repo_root: PathBuf::from("/work/repo"),
            repo_slug: None,
            head_sha: "abc123".to_owned(),
            tree_signature: "tree".to_owned(),
            config_signature: Some("config".to_owned()),
        });
        assert!(envelope.is_daemon_admissible());

        envelope.execution_owner = QueuedExecutionOwner::LegacyUnspecified;
        assert!(envelope.is_daemon_admissible());
        envelope.execution_owner = QueuedExecutionOwner::Foreground;
        assert!(!envelope.is_daemon_admissible());
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
            source_job_id: Some(envelope.job_id.clone()),
            schema_version: crate::ship_state::SHIP_STATE_SCHEMA_VERSION,
            merge_queue_observed_at: None,
            merge_queue_attempt_started_at: None,
            merge_queue_enqueue_succeeded_at: None,
            merge_queue_enqueue_started_at: None,
            abandoned: None,
        };
        let disposition = QueuedShipDisposition::new(
            QueuedShipDispositionKind::GreenValidationStateMissing,
            9,
            Some(&format!("{}\nsecret\0", "x".repeat(2_000))),
        );
        assert!(
            disposition
                .detail
                .as_ref()
                .is_some_and(|detail| detail.len() <= MAX_SHIP_POST_VALIDATION_DETAIL_BYTES)
        );
        assert!(
            disposition
                .detail
                .as_ref()
                .is_some_and(|detail| !detail.chars().any(char::is_control))
        );
        let outcome = QueuedExecutionOutcome::ship_with_post_validation(
            "job-ship",
            request.pr,
            state,
            true,
            disposition,
        );
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
            validation_build_type: None,
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
            validation_build_type: None,
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
        assert_eq!(
            plan.vm_slots,
            [VmSlotDemand {
                key: "macos".to_owned(),
                slots: 1,
            }]
        );
        assert!(plan.exclusive_claims.contains(&format!(
            "local-cwd:{}",
            temp.path().canonicalize().expect("canonical").display()
        )));
        assert!(plan.exclusive_claims.contains(&evidence_resource_claim(
            &run_evidence_scope(temp.path()),
            "feat/run",
            "mac"
        )));
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
        assert!(plan.exclusive_claims.contains(&evidence_resource_claim(
            &ship_evidence_scope(&request.repo, request.pr, Path::new("/work/repo")),
            "feat/ship",
            "mac"
        )));
    }

    #[test]
    fn ship_evidence_claims_isolate_prs_in_the_same_repository() {
        let modular = ship_request();
        let mut sequencer = modular.clone();
        sequencer.pr += 1;

        let modular_plan = JobResourcePlan::from_ship_request(Path::new("/work/repo"), &modular);
        let sequencer_plan =
            JobResourcePlan::from_ship_request(Path::new("/work/repo"), &sequencer);
        let evidence_claim = |plan: &JobResourcePlan| {
            plan.exclusive_claims
                .iter()
                .find(|claim| claim.starts_with("evidence:"))
                .cloned()
                .expect("evidence claim")
        };

        assert_ne!(
            evidence_claim(&modular_plan),
            evidence_claim(&sequencer_plan)
        );
    }

    #[test]
    fn ship_workload_and_resource_claims_canonicalize_repository_case() {
        let lower = ship_request();
        let mut upper = lower.clone();
        upper.repo = lower.repo.to_ascii_uppercase();

        let lower_envelope =
            QueuedExecutionEnvelope::from_ship_request("lower", "/work/repo", &lower);
        let upper_envelope =
            QueuedExecutionEnvelope::from_ship_request("upper", "/work/repo", &upper);

        assert_eq!(
            lower_envelope.workload_scope(),
            upper_envelope.workload_scope()
        );
        assert_eq!(
            lower_envelope.resource_plan.exclusive_claims,
            upper_envelope.resource_plan.exclusive_claims
        );
    }

    #[test]
    fn evidence_claims_allow_pulp_forge_products_and_vellum_to_run_in_parallel() {
        let request = run_request();
        let workload_paths = [
            Path::new("/work/pulp"),
            Path::new("/work/forge-modular"),
            Path::new("/work/forge-sequencer"),
            Path::new("/work/vellum"),
        ];

        let claims = workload_paths
            .iter()
            .map(|cwd| {
                JobResourcePlan::from_run_request(cwd, &request)
                    .exclusive_claims
                    .into_iter()
                    .find(|claim| claim.starts_with("evidence:"))
                    .expect("evidence claim")
            })
            .collect::<BTreeSet<_>>();

        assert_eq!(claims.len(), workload_paths.len());
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
        assert!(plan.vm_slots.is_empty());
        assert!(plan.exclusive_claims.contains(&evidence_resource_claim(
            &run_evidence_scope(Path::new("/work/repo")),
            "feat/run",
            "ubuntu"
        )));
        assert!(
            !plan
                .exclusive_claims
                .iter()
                .any(|claim| claim.contains("cloud"))
        );
    }

    #[test]
    fn github_hosted_macos_resource_plan_has_no_local_vm_slot_claim() {
        let mut request = run_request();
        request.targets = vec![cloud_target_with_platform("macos-15", "macos-arm64")];

        let plan = JobResourcePlan::from_run_request(Path::new("/work/repo"), &request);

        assert_eq!(plan.cloud_targets, ["macos-15"]);
        assert!(plan.vm_slots.is_empty());
    }

    #[test]
    fn host_pool_resource_plan_uses_pool_demand_not_member_claims() {
        let member = ssh_target("member-a", "mac-a", "/Users/shipyard/work/repo");
        let mut request = run_request();
        let target = ResolvedTarget {
            name: "mac-pool".to_owned(),
            validation_build_type: None,
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
        assert_eq!(
            plan.vm_slots,
            [VmSlotDemand {
                key: "macos".to_owned(),
                slots: 2,
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
            validation_build_type: None,
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

    #[test]
    fn current_queue_envelopes_fail_closed_on_identity_or_kind_drift() {
        let mut envelope =
            QueuedExecutionEnvelope::from_ship_request("job-42", "/work/repo", &ship_request());
        validate_queued_execution_envelope(envelope.clone()).expect("valid envelope");
        if let QueuedExecutionRequest::Ship(request) = &mut envelope.request {
            request.repo = "DanielRaffel/Shipyard".to_owned();
        }
        validate_queued_execution_envelope(envelope.clone()).expect("case-normalized repository");
        if let QueuedExecutionRequest::Ship(request) = &mut envelope.request {
            request.sha = "a".repeat(64);
        }
        validate_queued_execution_envelope(envelope.clone()).expect("full SHA-256 identity");
        if let QueuedExecutionRequest::Ship(request) = &mut envelope.request {
            request.sha = "A".repeat(40);
        }
        assert!(validate_queued_execution_envelope(envelope.clone()).is_err());
        if let QueuedExecutionRequest::Ship(request) = &mut envelope.request {
            request.sha = "a".repeat(40);
            request.repo = "owner/repo/extra".to_owned();
        }
        assert!(validate_queued_execution_envelope(envelope.clone()).is_err());
        if let QueuedExecutionRequest::Ship(request) = &mut envelope.request {
            request.repo = "DanielRaffel/Shipyard".to_owned();
        }

        envelope.job_id = "../escape".to_owned();
        assert!(validate_queued_execution_envelope(envelope.clone()).is_err());
        envelope.job_id = "job-42".to_owned();
        envelope.kind = QueuedExecutionKind::Run;
        assert!(validate_queued_execution_envelope(envelope.clone()).is_err());
        envelope.kind = QueuedExecutionKind::Ship;
        let QueuedExecutionRequest::Ship(request) = &mut envelope.request else {
            panic!("ship request");
        };
        request.pr = 0;
        assert!(validate_queued_execution_envelope(envelope).is_err());
    }
}
