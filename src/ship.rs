//! Ship execution orchestration helpers.
//!
//! The full `ship` command eventually ties together dispatch, queue,
//! evidence, ship-state, and merge behavior. This module starts with
//! the warm-pool and durable execution logic so executor wiring can
//! reuse it without embedding policy decisions in CLI code.

mod validation_outcome;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::capacity::{gather_configured_host_capacities, total_free};
use crate::config::LoadedConfig;
use crate::evidence::{
    EvidenceRecord, EvidenceStore, canonical_repository, run_evidence_scope, ship_evidence_scope,
};
use crate::executor::dispatch::{
    DispatchValidationRequest, ExecutorDispatcher, ResolvedBackend, ResolvedHostPoolConfig,
    ResolvedHostPoolMember, ResolvedTarget,
};
use crate::executor::streaming::{ProgressAction, ProgressEvent};
use crate::host_pool::{
    HostPoolConfig, HostPoolLeaseStore, HostPoolMemberConfig, default_lease_path,
};
use crate::identity::RuntimeMode;
use crate::job::{
    DEFAULT_RUNNING_JOB_STALE_SECONDS, Job, JobKind, JobStatus, JobTransitionError, Priority,
    TargetResult, TargetStatus, ValidationMode,
};
use crate::queue::{Queue, QueueDeferredRequeue, QueueError, STALE_RUNNING_CANCEL_REASON};
use crate::queue_request::{
    QueueOutcomeStore, QueueRequestError, QueueRequestResult, QueueRequestStore,
    QueuedExecutionEnvelope, QueuedExecutionKind, QueuedExecutionOutcome, QueuedExecutionOwner,
    QueuedShipDisposition,
};
use crate::queue_scheduler::{
    VmSlotCapacity, apply_admit_pass_for_drain, plan_admit_pass_from_jobs_with_vm_slots,
};
use crate::ship_state::{DispatchedRun, ShipState, ShipStatePrLock, ShipStateStore};
use crate::warm_pool::{
    PoolEntry, WarmPool, compute_expires_at, default_pool_path, is_backend_eligible, warm_host_key,
};

use validation_outcome::{
    completed_validation_disposition, persist_recovered_outcomes, policy_signature,
};
pub(crate) use validation_outcome::{persist_terminal_outcome, validation_proof_state};

const RESUME_ORDER: [&str; 4] = ["setup", "configure", "build", "test"];
const WARM_DEFAULT_RESUME_FROM: &str = "configure";
const DEFAULT_WORKDIR: &str = "~/repo";
const DEFAULT_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_DRAIN_MAX_WORKERS: usize = 2;
#[allow(clippy::duration_suboptimal_units)]
const QUEUE_ENVELOPE_SWEEP_GRACE: Duration = Duration::from_secs(60);

/// Resolved inputs for one `ship` execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShipExecutionRequest {
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
    /// Adopt the current head SHA when the recorded ship-state drifted — but
    /// ONLY when the amended commit has the same tree (e.g. a trailer-only
    /// `--amend`). The command layer verifies same-tree before setting this and
    /// refuses a content change, so prior evidence is never blessed for a
    /// different tree (Shipyard #346).
    pub adopt_head: bool,
    /// Optional PR snapshot file used to keep already-merged observation
    /// offline and deterministic. When `Some`, the admit-pass observation reads
    /// PR state from this path instead of shelling out to `gh` — mirroring the
    /// `auto_merge` escape-hatch pattern so tests do not hit the network.
    pub pr_snapshot_file: Option<PathBuf>,
    /// Ordered target list.
    pub targets: Vec<ResolvedTarget>,
}

/// Resolved inputs for one `shipyard run` execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunExecutionRequest {
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
    /// Ordered target list.
    pub targets: Vec<ResolvedTarget>,
}

/// Durable stores needed by ship execution.
pub struct ShipStores<'a> {
    /// Job queue store.
    pub queue: &'a mut Queue,
    /// Evidence store.
    pub evidence: &'a EvidenceStore,
    /// Ship-state store.
    pub ship_state: &'a ShipStateStore,
    /// Warm-pool store.
    pub warm_pool: &'a WarmPool,
    /// Original CLI working directory.
    pub cwd: &'a Path,
    /// State directory used for target logs.
    pub state_dir: &'a Path,
    /// Loaded Shipyard config used for cooperative scheduler admission.
    pub config: &'a LoadedConfig,
}

/// Durable stores needed by `shipyard run` execution.
pub struct RunStores<'a> {
    /// Job queue store.
    pub queue: &'a mut Queue,
    /// Evidence store.
    pub evidence: &'a EvidenceStore,
    /// Warm-pool store.
    pub warm_pool: &'a WarmPool,
    /// Original CLI working directory.
    pub cwd: &'a Path,
    /// State directory used for target logs.
    pub state_dir: &'a Path,
    /// Loaded Shipyard config used for cooperative scheduler admission.
    pub config: &'a LoadedConfig,
}

/// Outcome of one ship execution pass.
#[derive(Clone, Debug, PartialEq)]
pub struct ShipExecutionOutcome {
    /// Final job.
    pub job: Job,
    /// Final active ship state.
    pub ship_state: ShipState,
    /// Whether an existing compatible state was reused.
    pub resumed_existing_state: bool,
    /// Durable merge-readiness result kept separate from validation proof.
    pub post_validation: Option<QueuedShipDisposition>,
}

/// Outcome of one `shipyard run` execution.
#[derive(Clone, Debug, PartialEq)]
pub struct RunExecutionOutcome {
    /// Final job.
    pub job: Job,
}

/// Wait/retry controls for the cooperative drain loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CooperativeDrainOptions {
    /// Sleep duration between durable-state polls when another process owns
    /// the drain lock.
    pub poll_interval: Duration,
    /// Optional test/diagnostic cap on wait iterations.
    pub max_wait_iterations: Option<usize>,
}

impl Default for CooperativeDrainOptions {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_DRAIN_POLL_INTERVAL,
            max_wait_iterations: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum TargetExecutionOutcome {
    Completed(Job),
    Deferred { job: Job, reason: String },
}

impl TargetExecutionOutcome {
    fn into_completed(self) -> Result<Job, ShipExecutionError> {
        match self {
            Self::Completed(job) => Ok(job),
            Self::Deferred { reason, .. } => Err(ShipExecutionError::SchedulerDeferred(reason)),
        }
    }
}

/// Errors from ship execution orchestration.
#[derive(Debug)]
pub enum ShipExecutionError {
    /// Existing state belongs to a different SHA.
    ShaDrift {
        /// State SHA.
        existing: String,
        /// Current SHA.
        current: String,
    },
    /// Existing state was validated against a different base branch.
    BaseDrift {
        /// State base branch.
        existing: String,
        /// Current base branch.
        current: String,
    },
    /// Existing state was created under a different target/policy set.
    PolicyDrift {
        /// State policy signature.
        existing: String,
        /// Current policy signature.
        current: String,
    },
    /// Job transition failed.
    JobTransition(JobTransitionError),
    /// Queue persistence failed.
    Queue(QueueError),
    /// Queue request/outcome persistence failed.
    QueueRequest(QueueRequestError),
    /// Delayed-worker runtime setup failed before target execution.
    WorkerSetup(String),
    /// Evidence persistence failed.
    Evidence(String),
    /// Ship-state persistence failed.
    ShipState(String),
    /// Warm-pool persistence failed.
    WarmPool(std::io::Error),
    /// Worker observed a scheduler-owned transient deferral.
    SchedulerDeferred(String),
    /// Host-pool lease inspection failed during scheduler admission.
    HostPool(String),
    /// VM-slot capacity inspection failed during scheduler admission.
    VmSlot(String),
    /// A spawned drain worker failed to join.
    WorkerJoin(String),
    /// A matching same-PR ship job is already running.
    SamePrShipRunning {
        /// Repository slug.
        repo: String,
        /// Pull request number.
        pr: u64,
        /// Running queue job id.
        running_job_id: String,
    },
    /// Durable outcome was not found for a submitted job.
    MissingQueuedOutcome(String),
    /// Durable queue job was not found for a stored outcome.
    MissingQueuedJob(String),
    /// Durable outcome kind did not match the expected command.
    UnexpectedQueuedOutcome {
        /// Queue job id.
        job_id: String,
        /// Expected outcome kind.
        expected: &'static str,
    },
    /// Cooperative wait reached its configured limit.
    CooperativeWaitTimedOut(String),
}

impl Display for ShipExecutionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShaDrift { existing, current } => {
                write!(
                    formatter,
                    "ship state SHA drift: existing {existing}, current {current}"
                )
            }
            Self::BaseDrift { existing, current } => write!(
                formatter,
                "ship state base-branch drift: existing {existing}, current {current}"
            ),
            Self::PolicyDrift { existing, current } => write!(
                formatter,
                "ship state policy drift: existing {existing}, current {current}"
            ),
            Self::JobTransition(error) => write!(formatter, "{error}"),
            Self::Queue(error) => write!(formatter, "{error}"),
            Self::QueueRequest(error) => write!(formatter, "{error}"),
            Self::WorkerSetup(error) => write!(formatter, "worker setup failed: {error}"),
            Self::Evidence(error) => write!(formatter, "evidence write failed: {error}"),
            Self::ShipState(error) => write!(formatter, "ship-state write failed: {error}"),
            Self::WarmPool(error) => write!(formatter, "warm-pool write failed: {error}"),
            Self::SchedulerDeferred(reason) => {
                write!(formatter, "scheduler deferred validation: {reason}")
            }
            Self::HostPool(error) => write!(formatter, "host-pool scheduler read failed: {error}"),
            Self::VmSlot(error) => write!(formatter, "VM-slot scheduler read failed: {error}"),
            Self::WorkerJoin(job_id) => write!(formatter, "worker thread for {job_id} panicked"),
            Self::SamePrShipRunning {
                repo,
                pr,
                running_job_id,
            } => write!(
                formatter,
                "same-PR ship already running for {repo}#{pr} ({running_job_id}); use shipyard watch --pr {pr} or inspect shipyard queue/status"
            ),
            Self::MissingQueuedOutcome(job_id) => {
                write!(formatter, "queued outcome missing for job {job_id}")
            }
            Self::MissingQueuedJob(job_id) => {
                write!(formatter, "queued job missing for outcome {job_id}")
            }
            Self::UnexpectedQueuedOutcome { job_id, expected } => {
                write!(
                    formatter,
                    "queued outcome for job {job_id} is not a {expected} outcome"
                )
            }
            Self::CooperativeWaitTimedOut(job_id) => {
                write!(formatter, "timed out waiting for queued job {job_id}")
            }
        }
    }
}

impl Error for ShipExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::JobTransition(error) => Some(error),
            Self::Queue(error) => Some(error),
            Self::QueueRequest(error) => Some(error),
            Self::WarmPool(error) => Some(error),
            Self::ShaDrift { .. }
            | Self::BaseDrift { .. }
            | Self::PolicyDrift { .. }
            | Self::WorkerSetup(_)
            | Self::Evidence(_)
            | Self::ShipState(_)
            | Self::SchedulerDeferred(_)
            | Self::HostPool(_)
            | Self::VmSlot(_)
            | Self::WorkerJoin(_)
            | Self::SamePrShipRunning { .. }
            | Self::MissingQueuedOutcome(_)
            | Self::MissingQueuedJob(_)
            | Self::UnexpectedQueuedOutcome { .. }
            | Self::CooperativeWaitTimedOut(_) => None,
        }
    }
}

impl From<JobTransitionError> for ShipExecutionError {
    fn from(error: JobTransitionError) -> Self {
        Self::JobTransition(error)
    }
}

impl From<QueueError> for ShipExecutionError {
    fn from(error: QueueError) -> Self {
        Self::Queue(error)
    }
}

impl From<QueueRequestError> for ShipExecutionError {
    fn from(error: QueueRequestError) -> Self {
        Self::QueueRequest(error)
    }
}

/// Validation backend boundary used by ship orchestration.
pub trait ShipTargetDispatcher {
    /// Validate one resolved target.
    fn validate(&self, request: DispatchValidationRequest<'_, '_>) -> TargetResult;
}

impl ShipTargetDispatcher for ExecutorDispatcher {
    fn validate(&self, request: DispatchValidationRequest<'_, '_>) -> TargetResult {
        ExecutorDispatcher::validate(self, request)
    }
}

/// Execute all targets for a ship request and persist terminal state.
pub fn execute_ship<D: ShipTargetDispatcher>(
    request: &ShipExecutionRequest,
    stores: ShipStores<'_>,
    dispatcher: &D,
) -> Result<ShipExecutionOutcome, ShipExecutionError> {
    let ShipStores {
        queue,
        evidence,
        ship_state,
        warm_pool,
        cwd,
        state_dir,
        config,
    } = stores;
    let job = submit_ship(request, queue, cwd, state_dir)?;
    execute_ship_worker(
        request,
        job,
        ShipStores {
            queue,
            evidence,
            ship_state,
            warm_pool,
            cwd,
            state_dir,
            config,
        },
        dispatcher,
    )
}

/// Submit a `shipyard ship` request as a pending durable job.
pub fn submit_ship(
    request: &ShipExecutionRequest,
    queue: &mut Queue,
    cwd: &Path,
    state_dir: &Path,
) -> Result<Job, ShipExecutionError> {
    submit_ship_with_config(request, queue, cwd, state_dir, None)
}

/// Submit a ship request with configuration provenance for daemon ownership.
pub(crate) fn submit_ship_daemon(
    request: &ShipExecutionRequest,
    queue: &mut Queue,
    cwd: &Path,
    state_dir: &Path,
    config: &LoadedConfig,
) -> Result<Job, ShipExecutionError> {
    submit_ship_with_config(request, queue, cwd, state_dir, Some(config))
}

fn submit_ship_with_config(
    request: &ShipExecutionRequest,
    queue: &mut Queue,
    cwd: &Path,
    state_dir: &Path,
    config: Option<&LoadedConfig>,
) -> Result<Job, ShipExecutionError> {
    submit_ship_with_config_and_persist(
        request,
        queue,
        cwd,
        state_dir,
        config,
        save_submission_envelope,
    )
}

fn submit_ship_with_config_and_persist<P>(
    request: &ShipExecutionRequest,
    queue: &mut Queue,
    cwd: &Path,
    state_dir: &Path,
    config: Option<&LoadedConfig>,
    persist: P,
) -> Result<Job, ShipExecutionError>
where
    P: FnOnce(&QueueRequestStore, &QueuedExecutionEnvelope, bool) -> QueueRequestResult<()>,
{
    let workload_scope = format!(
        "ship:{}:pr-{}",
        canonical_repository(&request.repo),
        request.pr
    );
    // Queue-absence recovery uses this same short-lived workload fence while
    // it claims and commits a replacement. Hold it across the normal
    // submitter's running-owner check, durable envelope write, and queue
    // insertion so neither side can miss the other's pre-commit window.
    let _ownership_lock = queue.acquire_workload_admission_lock(&workload_scope)?;
    refuse_same_pr_running_ship(queue, state_dir, request)?;
    let target_names = target_names(&request.targets);
    let job = Job::create(
        request.sha.clone(),
        request.branch.clone(),
        target_names,
        request.mode,
        request.priority,
    )
    .with_kind(JobKind::Ship)
    .with_workload_scope(workload_scope);
    let request_store = QueueRequestStore::new(state_dir).map_err(QueueRequestError::from)?;
    let mut envelope = QueuedExecutionEnvelope::from_ship_request(job.id.clone(), cwd, request);
    let daemon_owned = config.is_some();
    if let Some(config) = config {
        envelope.execution_owner = QueuedExecutionOwner::Daemon;
        let provenance = crate::queue_request::ExecutionProvenance::capture_with_config(
            cwd,
            Some(&request.repo),
            &request.sha,
            config,
        )
        .ok_or_else(|| {
            ShipExecutionError::QueueRequest(QueueRequestError::InvalidSnapshot {
                reason: "exact unattended ship provenance changed before enqueue".to_owned(),
            })
        })?;
        envelope.cwd.clone_from(&provenance.canonical_cwd);
        envelope.provenance = Some(provenance);
    }
    persist(&request_store, &envelope, daemon_owned)?;
    if let Err(error) = queue.enqueue(job.clone()) {
        let _ = request_store.delete(&job.id);
        return Err(error.into());
    }
    Ok(job)
}

fn save_submission_envelope(
    request_store: &QueueRequestStore,
    envelope: &QueuedExecutionEnvelope,
    durable: bool,
) -> QueueRequestResult<()> {
    if durable {
        request_store.save_durable(envelope)
    } else {
        request_store.save(envelope)
    }
}

/// Load a completed `shipyard ship` outcome through the durable outcome store.
pub fn load_ship_outcome(
    queue: &mut Queue,
    state_dir: &Path,
    job_id: &str,
) -> Result<ShipExecutionOutcome, ShipExecutionError> {
    let Some(outcome) = QueueOutcomeStore::new(state_dir)
        .map_err(QueueRequestError::from)?
        .load(job_id)?
    else {
        return Err(ShipExecutionError::MissingQueuedOutcome(job_id.to_owned()));
    };
    let QueuedExecutionOutcome::Ship {
        ship_state,
        resumed_existing_state,
        post_validation,
        ..
    } = outcome
    else {
        return Err(ShipExecutionError::UnexpectedQueuedOutcome {
            job_id: job_id.to_owned(),
            expected: "ship",
        });
    };
    let Some(job) = queue.get(job_id)? else {
        return Err(ShipExecutionError::MissingQueuedJob(job_id.to_owned()));
    };
    Ok(ShipExecutionOutcome {
        job,
        ship_state,
        resumed_existing_state,
        post_validation,
    })
}

/// Wait for a submitted `shipyard ship` job, becoming the cooperative drain
/// owner when possible.
pub fn drain_or_wait_ship<D: ShipTargetDispatcher + Sync>(
    request: &ShipExecutionRequest,
    #[allow(clippy::needless_pass_by_value)] job: Job,
    stores: ShipStores<'_>,
    dispatcher: &D,
) -> Result<ShipExecutionOutcome, ShipExecutionError> {
    drain_or_wait_ship_with_scope(
        request,
        job,
        stores,
        dispatcher,
        CooperativeDrainOptions::default(),
        DrainScope::Cooperative,
    )
}

/// Wait for one explicitly requested `shipyard ship --pr` job without
/// executing or cancelling unrelated queued work. Scheduler resource and
/// priority decisions still account for the complete queue; only the awaited
/// job may be mutated by this drain owner.
pub fn drain_or_wait_ship_awaited_only<D: ShipTargetDispatcher + Sync>(
    request: &ShipExecutionRequest,
    #[allow(clippy::needless_pass_by_value)] job: Job,
    stores: ShipStores<'_>,
    dispatcher: &D,
) -> Result<ShipExecutionOutcome, ShipExecutionError> {
    drain_or_wait_ship_with_scope(
        request,
        job,
        stores,
        dispatcher,
        CooperativeDrainOptions::default(),
        DrainScope::AwaitedOnly,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrainScope {
    Cooperative,
    AwaitedOnly,
}

#[allow(clippy::needless_pass_by_value)]
fn drain_or_wait_ship_with_scope<D: ShipTargetDispatcher + Sync>(
    request: &ShipExecutionRequest,
    job: Job,
    stores: ShipStores<'_>,
    dispatcher: &D,
    options: CooperativeDrainOptions,
    drain_scope: DrainScope,
) -> Result<ShipExecutionOutcome, ShipExecutionError> {
    let ShipStores {
        queue,
        evidence,
        ship_state,
        warm_pool,
        cwd,
        state_dir,
        config,
    } = stores;
    let mut wait_iterations = 0usize;
    loop {
        if let Some(outcome) = terminal_ship_outcome(queue, state_dir, request, &job.id)? {
            return Ok(outcome);
        }
        if let Some(drain_lock) = queue.acquire_drain_lock()? {
            if drain_scope == DrainScope::Cooperative {
                let recovered =
                    recover_foreground_running_jobs_for_drain(queue, &drain_lock, state_dir)?;
                persist_recovered_outcomes(&recovered, state_dir, ship_state)?;
            }
            if let Some(outcome) = terminal_ship_outcome(queue, state_dir, request, &job.id)? {
                return Ok(outcome);
            }
            run_drain_worker_cycle_scoped(
                queue,
                &drain_lock,
                evidence,
                ship_state,
                warm_pool,
                cwd,
                state_dir,
                config,
                &job.id,
                dispatcher,
                request.pr_snapshot_file.as_deref(),
                drain_scope,
            )?;
        }
        wait_or_timeout(&job.id, &mut wait_iterations, options)?;
    }
}

/// Execute a previously submitted `shipyard ship` job.
pub fn execute_ship_worker<D: ShipTargetDispatcher>(
    request: &ShipExecutionRequest,
    job: Job,
    stores: ShipStores<'_>,
    dispatcher: &D,
) -> Result<ShipExecutionOutcome, ShipExecutionError> {
    execute_ship_worker_with_options(request, job, stores, dispatcher, false)
}

#[allow(clippy::too_many_lines)]
fn execute_ship_worker_with_options<D: ShipTargetDispatcher>(
    request: &ShipExecutionRequest,
    mut job: Job,
    stores: ShipStores<'_>,
    dispatcher: &D,
    defer_host_pool_lease_unavailable: bool,
) -> Result<ShipExecutionOutcome, ShipExecutionError> {
    let ShipStores {
        queue,
        evidence,
        ship_state,
        warm_pool,
        cwd,
        state_dir,
        config,
    } = stores;
    let reclassify_vitals_path = crate::host_health::incident_reclassify_path(config);
    let transient_retry = crate::ship_retry::transient_local_retry_policy(config);
    if let Some(requested) = durable_cancelled_job(queue, &job)? {
        let cancelled =
            if requested.status == JobStatus::Cancelled || defer_host_pool_lease_unavailable {
                requested
            } else {
                requested.cancel_with_reason(requested.cancellation_reason.clone())?
            };
        return Ok(ShipExecutionOutcome {
            job: cancelled,
            ship_state: unsaved_ship_state(request, &job.target_names),
            resumed_existing_state: false,
            post_validation: None,
        });
    }
    let ship_state_lock = ship_state
        .lock_pr_scoped(&request.repo, request.pr)
        .map_err(|error| ShipExecutionError::ShipState(error.to_string()))?;
    let resumed_existing_state = ship_state
        .get_locked_scoped(&request.repo, request.pr, &ship_state_lock)
        .is_some();
    let mut state = match load_or_create_state(
        request,
        &job.target_names,
        ship_state,
        Some(&ship_state_lock),
    ) {
        Ok(state) => state,
        Err(error) => {
            cancel_refused_job(queue, &job, &error)?;
            return Err(error);
        }
    };
    // Bind the durable ship attempt to the exact queue envelope that started
    // it. A later recovery must never infer ownership from repo/PR/SHA alone:
    // a fresh attempt may deliberately reuse all of those values.
    state.source_job_id = Some(job.id.clone());
    if let Err(error) = ship_state.save_scoped_locked(&state, &ship_state_lock) {
        let execution_error = ShipExecutionError::ShipState(error.to_string());
        cancel_refused_job(queue, &job, &execution_error)?;
        return Err(execution_error);
    }

    job = ensure_worker_running_job(queue, &job)?;

    job = execute_targets_with_options(
        request,
        state_dir,
        queue,
        warm_pool,
        dispatcher,
        job,
        TargetExecOptions {
            cwd,
            defer_host_pool_lease_unavailable,
            reclassify_vitals_path: reclassify_vitals_path.as_deref(),
            transient_retry,
            config: Some(config),
        },
    )?
    .into_completed()?;
    if job.cancel_requested_at.is_some() {
        if !defer_host_pool_lease_unavailable && job.status != JobStatus::Cancelled {
            job = job.cancel_with_reason(job.cancellation_reason.clone())?;
        }
        if !defer_host_pool_lease_unavailable {
            queue.update(&job)?;
            QueueOutcomeStore::new(state_dir)
                .map_err(QueueRequestError::from)?
                .save(&QueuedExecutionOutcome::ship(
                    job.id.clone(),
                    request.pr,
                    state.clone(),
                    resumed_existing_state,
                ))?;
        }
        return Ok(ShipExecutionOutcome {
            job,
            ship_state: state,
            resumed_existing_state,
            post_validation: None,
        });
    }
    job = job.complete()?;
    record_evidence(
        evidence,
        &ship_evidence_scope(&request.repo, request.pr, cwd),
        request,
        &job,
    )?;
    update_ship_state_from_job(&mut state, request, &job);
    ship_state
        .save_scoped_locked(&state, &ship_state_lock)
        .map_err(|error| ShipExecutionError::ShipState(error.to_string()))?;
    let post_validation = completed_validation_disposition(&job);
    QueueOutcomeStore::new(state_dir)
        .map_err(QueueRequestError::from)?
        .save(&QueuedExecutionOutcome::ship_with_post_validation(
            job.id.clone(),
            request.pr,
            state.clone(),
            resumed_existing_state,
            post_validation.clone(),
        ))?;
    queue.update(&job)?;

    Ok(ShipExecutionOutcome {
        job,
        ship_state: state,
        resumed_existing_state,
        post_validation: Some(post_validation),
    })
}

/// Execute configured targets for `shipyard run` without PR/ship-state mutation.
pub fn execute_run<D: ShipTargetDispatcher>(
    request: &RunExecutionRequest,
    stores: RunStores<'_>,
    dispatcher: &D,
) -> Result<RunExecutionOutcome, ShipExecutionError> {
    let RunStores {
        queue,
        evidence,
        warm_pool,
        cwd,
        state_dir,
        config,
    } = stores;
    let job = submit_run(request, queue, cwd, state_dir)?;
    execute_run_worker(
        request,
        job,
        RunStores {
            queue,
            evidence,
            warm_pool,
            cwd,
            state_dir,
            config,
        },
        dispatcher,
    )
}

/// Submit a `shipyard run` request as a pending durable job.
pub fn submit_run(
    request: &RunExecutionRequest,
    queue: &mut Queue,
    cwd: &Path,
    state_dir: &Path,
) -> Result<Job, ShipExecutionError> {
    submit_run_with_config(request, queue, cwd, state_dir, None)
}

/// Submit a run request with configuration provenance for daemon ownership.
pub(crate) fn submit_run_daemon(
    request: &RunExecutionRequest,
    queue: &mut Queue,
    cwd: &Path,
    state_dir: &Path,
    config: &LoadedConfig,
) -> Result<Job, ShipExecutionError> {
    submit_run_with_config(request, queue, cwd, state_dir, Some(config))
}

fn submit_run_with_config(
    request: &RunExecutionRequest,
    queue: &mut Queue,
    cwd: &Path,
    state_dir: &Path,
    config: Option<&LoadedConfig>,
) -> Result<Job, ShipExecutionError> {
    let target_names = target_names(&request.targets);
    let job = Job::create(
        request.sha.clone(),
        request.branch.clone(),
        target_names,
        request.mode,
        request.priority,
    )
    .with_kind(JobKind::Run)
    .with_workload_scope(crate::queue_request::run_workload_scope(cwd));
    let request_store = QueueRequestStore::new(state_dir).map_err(QueueRequestError::from)?;
    let mut envelope = QueuedExecutionEnvelope::from_run_request(job.id.clone(), cwd, request);
    let daemon_owned = config.is_some();
    if let Some(config) = config {
        envelope.execution_owner = QueuedExecutionOwner::Daemon;
        let provenance = crate::queue_request::ExecutionProvenance::capture_with_config(
            cwd,
            None,
            &request.sha,
            config,
        )
        .ok_or_else(|| {
            ShipExecutionError::QueueRequest(QueueRequestError::InvalidSnapshot {
                reason: "exact unattended run provenance changed before enqueue".to_owned(),
            })
        })?;
        envelope.cwd.clone_from(&provenance.canonical_cwd);
        envelope.provenance = Some(provenance);
    }
    save_submission_envelope(&request_store, &envelope, daemon_owned)?;
    if let Err(error) = queue.enqueue(job.clone()) {
        let _ = request_store.delete(&job.id);
        return Err(error.into());
    }
    Ok(job)
}

/// Load a completed `shipyard run` outcome through the durable outcome store.
pub fn load_run_outcome(
    queue: &mut Queue,
    state_dir: &Path,
    job_id: &str,
) -> Result<RunExecutionOutcome, ShipExecutionError> {
    let Some(outcome) = QueueOutcomeStore::new(state_dir)
        .map_err(QueueRequestError::from)?
        .load(job_id)?
    else {
        return Err(ShipExecutionError::MissingQueuedOutcome(job_id.to_owned()));
    };
    if !matches!(outcome, QueuedExecutionOutcome::Run { .. }) {
        return Err(ShipExecutionError::UnexpectedQueuedOutcome {
            job_id: job_id.to_owned(),
            expected: "run",
        });
    }
    let Some(job) = queue.get(job_id)? else {
        return Err(ShipExecutionError::MissingQueuedJob(job_id.to_owned()));
    };
    Ok(RunExecutionOutcome { job })
}

/// Wait for a submitted `shipyard run` job, becoming the cooperative drain
/// owner when possible.
pub fn drain_or_wait_run<D: ShipTargetDispatcher + Sync>(
    request: &RunExecutionRequest,
    job: Job,
    stores: RunStores<'_>,
    dispatcher: &D,
) -> Result<RunExecutionOutcome, ShipExecutionError> {
    drain_or_wait_run_with_options(
        request,
        job,
        stores,
        dispatcher,
        CooperativeDrainOptions::default(),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn drain_or_wait_run_with_options<D: ShipTargetDispatcher + Sync>(
    _request: &RunExecutionRequest,
    job: Job,
    stores: RunStores<'_>,
    dispatcher: &D,
    options: CooperativeDrainOptions,
) -> Result<RunExecutionOutcome, ShipExecutionError> {
    let RunStores {
        queue,
        evidence,
        warm_pool,
        cwd,
        state_dir,
        config,
    } = stores;
    let mut wait_iterations = 0usize;
    loop {
        if let Some(outcome) = terminal_run_outcome(queue, state_dir, &job.id)? {
            return Ok(outcome);
        }
        if let Some(drain_lock) = queue.acquire_drain_lock()? {
            let ship_state = ShipStateStore::new(state_dir.join("ship"))
                .map_err(|error| ShipExecutionError::ShipState(error.to_string()))?;
            let recovered =
                recover_foreground_running_jobs_for_drain(queue, &drain_lock, state_dir)?;
            persist_recovered_outcomes(&recovered, state_dir, &ship_state)?;
            if let Some(outcome) = terminal_run_outcome(queue, state_dir, &job.id)? {
                return Ok(outcome);
            }
            run_drain_worker_cycle(
                queue,
                &drain_lock,
                evidence,
                &ship_state,
                warm_pool,
                cwd,
                state_dir,
                config,
                &job.id,
                dispatcher,
                None,
            )?;
        }
        wait_or_timeout(&job.id, &mut wait_iterations, options)?;
    }
}

/// Execute a previously submitted `shipyard run` job.
pub fn execute_run_worker<D: ShipTargetDispatcher>(
    request: &RunExecutionRequest,
    job: Job,
    stores: RunStores<'_>,
    dispatcher: &D,
) -> Result<RunExecutionOutcome, ShipExecutionError> {
    execute_run_worker_with_options(request, job, stores, dispatcher, false)
}

fn execute_run_worker_with_options<D: ShipTargetDispatcher>(
    request: &RunExecutionRequest,
    mut job: Job,
    stores: RunStores<'_>,
    dispatcher: &D,
    defer_host_pool_lease_unavailable: bool,
) -> Result<RunExecutionOutcome, ShipExecutionError> {
    let RunStores {
        queue,
        evidence,
        warm_pool,
        cwd,
        state_dir,
        config,
    } = stores;
    let reclassify_vitals_path = crate::host_health::incident_reclassify_path(config);
    let transient_retry = crate::ship_retry::transient_local_retry_policy(config);
    if let Some(requested) = durable_cancelled_job(queue, &job)? {
        let cancelled =
            if requested.status == JobStatus::Cancelled || defer_host_pool_lease_unavailable {
                requested
            } else {
                requested.cancel_with_reason(requested.cancellation_reason.clone())?
            };
        return Ok(RunExecutionOutcome { job: cancelled });
    }
    let shim = ShipExecutionRequest {
        pr: 0,
        repo: String::new(),
        branch: request.branch.clone(),
        base_branch: String::new(),
        sha: request.sha.clone(),
        commit_subject: String::new(),
        pr_url: None,
        pr_title: None,
        mode: request.mode,
        priority: request.priority,
        warm_disabled: request.warm_disabled,
        fail_fast: request.fail_fast,
        resume_from: request.resume_from.clone(),
        advisory_targets: BTreeSet::new(),
        adopt_head: false,
        pr_snapshot_file: None,
        targets: request.targets.clone(),
    };
    job = ensure_worker_running_job(queue, &job)?;
    job = execute_targets_with_options(
        &shim,
        state_dir,
        queue,
        warm_pool,
        dispatcher,
        job,
        TargetExecOptions {
            cwd,
            defer_host_pool_lease_unavailable,
            reclassify_vitals_path: reclassify_vitals_path.as_deref(),
            transient_retry,
            config: Some(config),
        },
    )?
    .into_completed()?;
    if job.cancel_requested_at.is_some() {
        if !defer_host_pool_lease_unavailable && job.status != JobStatus::Cancelled {
            job = job.cancel_with_reason(job.cancellation_reason.clone())?;
        }
        if !defer_host_pool_lease_unavailable {
            queue.update(&job)?;
            QueueOutcomeStore::new(state_dir)
                .map_err(QueueRequestError::from)?
                .save(&QueuedExecutionOutcome::run(job.id.clone()))?;
        }
        return Ok(RunExecutionOutcome { job });
    }
    job = job.complete()?;
    record_evidence(evidence, &run_evidence_scope(cwd), &shim, &job)?;
    QueueOutcomeStore::new(state_dir)
        .map_err(QueueRequestError::from)?
        .save(&QueuedExecutionOutcome::run(job.id.clone()))?;
    queue.update(&job)?;
    Ok(RunExecutionOutcome { job })
}

fn cancel_refused_job(
    queue: &mut Queue,
    job: &Job,
    error: &ShipExecutionError,
) -> Result<(), ShipExecutionError> {
    let cancelled = job.cancel_with_reason(Some(error.to_string()))?;
    queue.update(&cancelled)?;
    Ok(())
}

fn durable_cancelled_job(queue: &mut Queue, job: &Job) -> Result<Option<Job>, ShipExecutionError> {
    let Some(durable) = queue.get(&job.id)? else {
        return Ok(None);
    };
    match durable.status {
        JobStatus::Cancelled => Ok(Some(durable)),
        JobStatus::Running if durable.cancel_requested_at.is_some() => Ok(Some(durable)),
        JobStatus::Pending | JobStatus::Running | JobStatus::Completed => Ok(None),
    }
}

fn scheduler_vm_slots(
    config: &LoadedConfig,
    jobs: &[Job],
    request_store: &QueueRequestStore,
) -> Result<Vec<VmSlotCapacity>, ShipExecutionError> {
    if !jobs_use_vm_slot(jobs, request_store, "macos") {
        return Ok(Vec::new());
    }
    let hosts =
        gather_configured_host_capacities(&config.data).map_err(ShipExecutionError::VmSlot)?;
    if hosts.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![VmSlotCapacity {
        key: "macos".to_owned(),
        slots: total_free(&hosts),
    }])
}

fn scheduler_host_pool_leases(
    jobs: &[Job],
    request_store: &QueueRequestStore,
    pools: &[HostPoolConfig],
    leases: Vec<crate::host_pool::HostPoolLease>,
) -> Vec<crate::host_pool::HostPoolLease> {
    let pool_map = pools
        .iter()
        .map(|pool| (pool.name.as_str(), pool))
        .collect::<BTreeMap<_, _>>();
    let mut reservations = Vec::new();
    for job in jobs.iter().filter(|job| job.status == JobStatus::Running) {
        let Ok(Some(envelope)) = request_store.load(&job.id) else {
            continue;
        };
        for demand in envelope.resource_plan.host_pools {
            let Some(pool) = pool_map.get(demand.pool_name.as_str()) else {
                continue;
            };
            let eligible_members = pool
                .members
                .iter()
                .filter(|member| {
                    demand.requires.iter().all(|required| {
                        member
                            .capabilities
                            .iter()
                            .any(|capability| capability == required)
                    })
                })
                .map(|member| member.id.clone())
                .collect();
            reservations.push((
                job.id.clone(),
                demand.pool_name,
                eligible_members,
                demand.slots,
            ));
        }
    }
    leases_not_covered_by_running_reservations(&mut reservations, leases)
}

fn leases_not_covered_by_running_reservations(
    reservations: &mut [(String, String, BTreeSet<String>, u32)],
    leases: Vec<crate::host_pool::HostPoolLease>,
) -> Vec<crate::host_pool::HostPoolLease> {
    leases
        .into_iter()
        .filter(|lease| {
            let Some(job_id) = lease.job_id.as_ref() else {
                return true;
            };
            let Some((_, _, _, remaining)) = reservations.iter_mut().find(
                |(reserved_job_id, pool_name, eligible_members, remaining)| {
                    *remaining > 0
                        && reserved_job_id == job_id
                        && pool_name == &lease.pool_name
                        && eligible_members.contains(&lease.member_id)
                },
            ) else {
                return true;
            };
            *remaining -= 1;
            false
        })
        .collect()
}

fn jobs_use_vm_slot(jobs: &[Job], request_store: &QueueRequestStore, key: &str) -> bool {
    jobs.iter()
        .filter(|job| matches!(job.status, JobStatus::Pending | JobStatus::Running))
        .any(|job| {
            request_store
                .load(&job.id)
                .ok()
                .flatten()
                .is_some_and(|envelope| {
                    envelope
                        .resource_plan
                        .vm_slots
                        .iter()
                        .any(|slot| slot.key == key)
                })
        })
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)] // One drain loop owns the shared observation cache.
fn run_drain_worker_cycle<D: ShipTargetDispatcher + Sync>(
    queue: &mut Queue,
    drain_lock: &crate::queue::DrainLock,
    evidence: &EvidenceStore,
    ship_state: &ShipStateStore,
    warm_pool: &WarmPool,
    cwd: &Path,
    state_dir: &Path,
    config: &LoadedConfig,
    awaited_job_id: &str,
    dispatcher: &D,
    pr_snapshot_file: Option<&Path>,
) -> Result<(), ShipExecutionError> {
    run_drain_worker_cycle_scoped(
        queue,
        drain_lock,
        evidence,
        ship_state,
        warm_pool,
        cwd,
        state_dir,
        config,
        awaited_job_id,
        dispatcher,
        pr_snapshot_file,
        DrainScope::Cooperative,
    )
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)] // One drain loop owns the shared observation cache.
fn run_drain_worker_cycle_scoped<D: ShipTargetDispatcher + Sync>(
    queue: &mut Queue,
    drain_lock: &crate::queue::DrainLock,
    evidence: &EvidenceStore,
    ship_state: &ShipStateStore,
    warm_pool: &WarmPool,
    cwd: &Path,
    state_dir: &Path,
    config: &LoadedConfig,
    awaited_job_id: &str,
    dispatcher: &D,
    pr_snapshot_file: Option<&Path>,
    drain_scope: DrainScope,
) -> Result<(), ShipExecutionError> {
    let request_store = QueueRequestStore::new(state_dir).map_err(QueueRequestError::from)?;
    let outcome_store = QueueOutcomeStore::new(state_dir).map_err(QueueRequestError::from)?;
    let _trimmed_job_ids = queue.trim_terminal_jobs_for_drain(drain_lock)?;
    let jobs = queue.get_all()?;
    sweep_absent_queue_envelopes(state_dir, &jobs, &request_store, &outcome_store)?;
    let queue_state_dir = queue.state_dir().to_path_buf();
    let mut already_merged_observer =
        crate::queue_scheduler::AlreadyMergedObserver::from_config(config);
    thread::scope(|scope| -> Result<(), ShipExecutionError> {
        let (completion_tx, completion_rx) = mpsc::channel();
        let mut handles = Vec::new();
        let mut active_workers = 0usize;
        let mut first_error = None;
        let mut refill_allowed = true;

        loop {
            if refill_allowed && first_error.is_none() {
                refill_allowed = awaited_job_allows_refill(queue, awaited_job_id, &mut first_error);
            }
            if refill_allowed && first_error.is_none() {
                let worker_inputs = match admit_drain_worker_inputs(
                    queue,
                    drain_lock,
                    state_dir,
                    config,
                    &request_store,
                    awaited_job_id,
                    cwd,
                    pr_snapshot_file,
                    &mut already_merged_observer,
                    drain_scope,
                ) {
                    Ok(worker_inputs) => worker_inputs,
                    Err(error) => {
                        first_error = Some(error);
                        Vec::new()
                    }
                };
                for (job, envelope) in worker_inputs {
                    let job_id = job.id.clone();
                    let completion_tx = completion_tx.clone();
                    let evidence = evidence.clone();
                    let ship_state = ship_state.clone();
                    let warm_pool = warm_pool.clone();
                    let state_dir = state_dir.to_path_buf();
                    let queue_state_dir = queue_state_dir.clone();
                    let fallback_cwd = cwd.to_path_buf();
                    active_workers += 1;
                    handles.push((
                        job_id.clone(),
                        scope.spawn(move || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    run_started_worker(
                                        job,
                                        envelope,
                                        &evidence,
                                        &ship_state,
                                        &warm_pool,
                                        &fallback_cwd,
                                        &queue_state_dir,
                                        &state_dir,
                                        config,
                                        dispatcher,
                                    )
                                }))
                                .unwrap_or_else(|_| {
                                    Err(ShipExecutionError::WorkerJoin(job_id.clone()))
                                });
                            let _ = completion_tx.send((job_id, result));
                        }),
                    ));
                }
            }

            if active_workers == 0 {
                break;
            }

            let (job_id, result) = completion_rx
                .recv()
                .expect("active drain worker must report completion");
            active_workers -= 1;
            let handle_index = handles
                .iter()
                .position(|(handle_job_id, _)| handle_job_id == &job_id)
                .expect("reported drain worker must have a join handle");
            let (handle_job_id, handle) = handles.swap_remove(handle_index);
            if handle.join().is_err() && first_error.is_none() {
                first_error = Some(ShipExecutionError::WorkerJoin(handle_job_id));
            }
            refill_allowed &= apply_drain_worker_completion(
                queue,
                drain_lock,
                awaited_job_id,
                job_id,
                result,
                &mut first_error,
            );
        }

        for (job_id, handle) in handles {
            if handle.join().is_err() && first_error.is_none() {
                first_error = Some(ShipExecutionError::WorkerJoin(job_id));
            }
        }
        first_error.map_or(Ok(()), Err)
    })
}

fn apply_drain_worker_completion(
    queue: &mut Queue,
    drain_lock: &crate::queue::DrainLock,
    awaited_job_id: &str,
    job_id: String,
    result: Result<(), ShipExecutionError>,
    first_error: &mut Option<ShipExecutionError>,
) -> bool {
    match result {
        Err(ShipExecutionError::SchedulerDeferred(reason)) => {
            let requeue_result = queue.requeue_deferred_running_jobs_for_drain(
                drain_lock,
                &[QueueDeferredRequeue {
                    job_id,
                    reason,
                    defer_until: Some(defer_until(Utc::now())),
                }],
            );
            match requeue_result {
                Err(error) if first_error.is_none() => *first_error = Some(error.into()),
                Ok(requeued) => {
                    for job in requeued
                        .iter()
                        .filter(|job| job.status == JobStatus::Cancelled)
                    {
                        if let Err(error) = persist_terminal_outcome(job, queue.state_dir())
                            && first_error.is_none()
                        {
                            *first_error = Some(error);
                        }
                    }
                }
                Err(_) => {}
            }
        }
        Err(error) if first_error.is_none() => *first_error = Some(error),
        Ok(()) | Err(_) => {}
    }
    awaited_job_allows_refill(queue, awaited_job_id, first_error)
}

fn awaited_job_allows_refill(
    queue: &mut Queue,
    awaited_job_id: &str,
    first_error: &mut Option<ShipExecutionError>,
) -> bool {
    match queue.get(awaited_job_id) {
        Ok(Some(job)) if matches!(job.status, JobStatus::Completed | JobStatus::Cancelled) => false,
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            if first_error.is_none() {
                *first_error = Some(error.into());
            }
            false
        }
    }
}

#[allow(clippy::too_many_arguments)] // Scheduler state is passed explicitly at the drain boundary.
fn admit_drain_worker_inputs(
    queue: &mut Queue,
    drain_lock: &crate::queue::DrainLock,
    state_dir: &Path,
    config: &LoadedConfig,
    request_store: &QueueRequestStore,
    awaited_job_id: &str,
    cwd: &Path,
    pr_snapshot_file: Option<&Path>,
    already_merged_observer: &mut crate::queue_scheduler::AlreadyMergedObserver,
    scope: DrainScope,
) -> Result<Vec<(Job, QueuedExecutionEnvelope)>, ShipExecutionError> {
    let jobs = queue.get_all()?;
    let pools = scheduler_host_pools(&jobs, request_store)?;
    let leases = HostPoolLeaseStore::new(default_lease_path(state_dir))
        .leases()
        .map_err(|error| ShipExecutionError::HostPool(error.to_string()))?;
    let leases = scheduler_host_pool_leases(&jobs, request_store, &pools, leases);
    let vm_slots = scheduler_vm_slots(config, &jobs, request_store)?;
    let now = Utc::now();
    let mut scheduling_jobs = jobs.clone();
    scheduling_jobs.retain(|job| {
        job.status != JobStatus::Pending
            || matches!(
                request_store.load(&job.id),
                Ok(Some(envelope))
                    if envelope.job_id == job.id && envelope.is_foreground_owned()
            )
    });
    if scope == DrainScope::AwaitedOnly {
        // The shared planner normally frees claims held by stale same-PR
        // workers because the cooperative apply pass will reap them. A scoped
        // caller refuses to mutate unrelated jobs, so keep every unrelated
        // running job live in this in-memory planning snapshot and preserve
        // its claims. This never alters durable queue state.
        for job in scheduling_jobs
            .iter_mut()
            .filter(|job| job.id != awaited_job_id && job.status == JobStatus::Running)
        {
            job.started_at = Some(now);
        }
    }
    let mut pass = plan_admit_pass_from_jobs_with_vm_slots(
        &scheduling_jobs,
        request_store,
        &pools,
        &leases,
        &vm_slots,
        now,
    );
    let awaited_observation_jobs;
    let observation_jobs = if scope == DrainScope::AwaitedOnly {
        awaited_observation_jobs = jobs
            .iter()
            .filter(|job| {
                job.id == awaited_job_id
                    && scheduling_jobs
                        .iter()
                        .any(|scheduled| scheduled.id == job.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        awaited_observation_jobs.as_slice()
    } else {
        scheduling_jobs.as_slice()
    };
    pass.already_merged_cancellations = already_merged_observer.observe_pending(
        observation_jobs,
        request_store,
        cwd,
        pr_snapshot_file,
    );
    if scope == DrainScope::AwaitedOnly {
        restrict_admit_pass_to_awaited(&mut pass, awaited_job_id);
    }
    cap_admit_pass_workers(&jobs, &mut pass, DEFAULT_DRAIN_MAX_WORKERS);
    let awaited_will_be_cancelled = pass
        .plan
        .orphaned
        .iter()
        .any(|orphan| orphan.job_id == awaited_job_id)
        || pass
            .same_pr_ship_admission
            .pending_cancellations
            .iter()
            .any(|cancellation| cancellation.job_id == awaited_job_id)
        || pass
            .same_pr_ship_admission
            .stale_running_cancellations
            .iter()
            .any(|cancellation| cancellation.job_id == awaited_job_id);
    if awaited_will_be_cancelled {
        pass.plan.admitted.clear();
    }
    let applied = apply_admit_pass_for_drain(queue, drain_lock, &pass)?;
    applied
        .started
        .into_iter()
        .map(|job| {
            let envelope = request_store
                .load(&job.id)?
                .ok_or_else(|| ShipExecutionError::MissingQueuedJob(job.id.clone()))?;
            Ok((job, envelope))
        })
        .collect()
}

fn restrict_admit_pass_to_awaited(
    pass: &mut crate::queue_scheduler::RequestBackedAdmitPass,
    awaited_job_id: &str,
) {
    pass.plan.admitted.retain(|job_id| job_id == awaited_job_id);
    pass.plan
        .deferred
        .retain(|deferred| deferred.job_id == awaited_job_id);
    pass.plan
        .orphaned
        .retain(|orphan| orphan.job_id == awaited_job_id);
    pass.same_pr_ship_admission
        .pending_cancellations
        .retain(|cancellation| cancellation.job_id == awaited_job_id);
    pass.same_pr_ship_admission
        .running_conflicts
        .retain(|conflict| conflict.pending_job_id == awaited_job_id);
    pass.same_pr_ship_admission
        .stale_running_cancellations
        .retain(|cancellation| cancellation.job_id == awaited_job_id);
    pass.already_merged_cancellations
        .retain(|cancellation| cancellation.job_id == awaited_job_id);
}

fn sweep_absent_queue_envelopes(
    state_dir: &Path,
    jobs: &[Job],
    request_store: &QueueRequestStore,
    outcome_store: &QueueOutcomeStore,
) -> Result<(), ShipExecutionError> {
    let active_job_ids = jobs
        .iter()
        .map(|job| job.id.clone())
        .collect::<BTreeSet<_>>();
    let mut retained_request_ids = active_job_ids.clone();
    retained_request_ids.extend(crate::queue_absent_recovery::protected_request_job_ids(
        state_dir,
        request_store,
    )?);
    request_store.sweep_absent_older_than(&retained_request_ids, QUEUE_ENVELOPE_SWEEP_GRACE)?;
    outcome_store.sweep_absent_older_than(&retained_request_ids, QUEUE_ENVELOPE_SWEEP_GRACE)?;
    Ok(())
}

fn cap_admit_pass_workers(
    jobs: &[Job],
    pass: &mut crate::queue_scheduler::RequestBackedAdmitPass,
    max_workers: usize,
) {
    let running = jobs
        .iter()
        .filter(|job| job.status == JobStatus::Running)
        .count();
    let available = max_workers.saturating_sub(running);
    pass.plan.admitted.truncate(available);
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
pub(crate) fn run_started_worker<D: ShipTargetDispatcher>(
    job: Job,
    envelope: QueuedExecutionEnvelope,
    evidence: &EvidenceStore,
    ship_state: &ShipStateStore,
    warm_pool: &WarmPool,
    fallback_cwd: &Path,
    queue_state_dir: &Path,
    state_dir: &Path,
    config: &LoadedConfig,
    dispatcher: &D,
) -> Result<(), ShipExecutionError> {
    let worker_cwd = envelope.cwd.as_path();
    let cwd = if worker_cwd.as_os_str().is_empty() {
        fallback_cwd
    } else {
        worker_cwd
    };
    let mut worker_queue = Queue::new(queue_state_dir).map_err(QueueError::from)?;
    match envelope.kind {
        QueuedExecutionKind::Run => {
            let request = envelope.to_run_request()?;
            execute_run_worker_with_options(
                &request,
                job,
                RunStores {
                    queue: &mut worker_queue,
                    evidence,
                    warm_pool,
                    cwd,
                    state_dir,
                    config,
                },
                dispatcher,
                true,
            )?;
        }
        QueuedExecutionKind::Ship => {
            let request = envelope.to_ship_request()?;
            execute_ship_worker_with_options(
                &request,
                job,
                ShipStores {
                    queue: &mut worker_queue,
                    evidence,
                    ship_state,
                    warm_pool,
                    cwd,
                    state_dir,
                    config,
                },
                dispatcher,
                true,
            )?;
        }
    }
    Ok(())
}

/// Execute one queue job already fenced to `Running` by the daemon supervisor.
/// The delayed worker reloads policy from the submitted repository, validates
/// immutable checkout provenance, and never falls back to the process cwd.
pub(crate) fn execute_started_queued_job(
    job_id: &str,
    mode: RuntimeMode,
    global_dir: &Path,
    state_dir: &Path,
) -> Result<(QueuedExecutionKind, Job), ShipExecutionError> {
    let request_store = QueueRequestStore::new(state_dir).map_err(QueueRequestError::from)?;
    let envelope = request_store
        .load(job_id)?
        .ok_or_else(|| ShipExecutionError::MissingQueuedJob(job_id.to_owned()))?;
    if envelope.job_id != job_id {
        return Err(ShipExecutionError::QueueRequest(
            QueueRequestError::InvalidSnapshot {
                reason: format!(
                    "queued execution request for {job_id} belongs to {}",
                    envelope.job_id
                ),
            },
        ));
    }
    let provenance = envelope.provenance.as_ref().ok_or_else(|| {
        ShipExecutionError::QueueRequest(QueueRequestError::InvalidSnapshot {
            reason: "legacy request lacks unattended-execution provenance".to_owned(),
        })
    })?;
    let canonical_cwd = provenance.canonical_cwd.clone();
    let config =
        LoadedConfig::load_from_cwd_with_global_dir(mode, &canonical_cwd, global_dir.to_path_buf())
            .map_err(|error| ShipExecutionError::WorkerSetup(error.to_string()))?;
    provenance.validate_with_config(&canonical_cwd, &config)?;
    if matches!(envelope.kind, QueuedExecutionKind::Ship)
        && config.get_str("github.auth.source") != Some("command")
    {
        return Err(ShipExecutionError::WorkerSetup(
            "daemon-owned ship requires github.auth.source = command; env and ambient gh auth are forbidden"
                .to_owned(),
        ));
    }
    let mut queue = Queue::new(state_dir).map_err(QueueError::from)?;
    let job = queue
        .get(job_id)?
        .ok_or_else(|| ShipExecutionError::MissingQueuedJob(job_id.to_owned()))?;
    if job.status != JobStatus::Running {
        return Err(ShipExecutionError::QueueRequest(
            QueueRequestError::InvalidSnapshot {
                reason: format!("daemon worker requires running job, found {:?}", job.status),
            },
        ));
    }
    let evidence = EvidenceStore::new(state_dir.join("evidence"))
        .map_err(|error| ShipExecutionError::Evidence(error.to_string()))?;
    let ship_state = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| ShipExecutionError::ShipState(error.to_string()))?;
    let warm_pool = WarmPool::new(default_pool_path(state_dir));
    let prepared = crate::prepared_state::PreparedStateStore::new(state_dir.join("prepared"))
        .map_err(|error| ShipExecutionError::WorkerSetup(error.to_string()))?;
    let dispatcher = ExecutorDispatcher::new_with_state_dir_and_log_retention(
        Some(prepared),
        state_dir,
        crate::log_retention::LogRetentionPolicy::from_config(&config),
    );
    let mut envelope = envelope;
    envelope.cwd = canonical_cwd;
    run_started_worker(
        job,
        envelope.clone(),
        &evidence,
        &ship_state,
        &warm_pool,
        &envelope.cwd,
        state_dir,
        state_dir,
        &config,
        &dispatcher,
    )?;
    let completed = queue
        .get(job_id)?
        .ok_or_else(|| ShipExecutionError::MissingQueuedJob(job_id.to_owned()))?;
    Ok((envelope.kind, completed))
}

fn defer_until(now: DateTime<Utc>) -> DateTime<Utc> {
    now + chrono::Duration::seconds(5)
}

fn recover_foreground_running_jobs_for_drain(
    queue: &mut Queue,
    drain_lock: &crate::queue::DrainLock,
    state_dir: &Path,
) -> Result<Vec<Job>, ShipExecutionError> {
    let request_store = QueueRequestStore::new(state_dir).map_err(QueueRequestError::from)?;
    let recoverable = queue
        .get_running()?
        .into_iter()
        .filter_map(|job| match request_store.load(&job.id) {
            Ok(Some(envelope)) if envelope.job_id == job.id && envelope.is_foreground_owned() => {
                Some(job.id)
            }
            // Missing, corrupt, or daemon-owned envelopes are unknown or
            // externally owned. A foreground drain must preserve them; the
            // scheduler will also block new admission while their claims are
            // unknowable.
            Ok(Some(_) | None) | Err(_) => None,
        })
        .collect::<Vec<_>>();
    queue
        .recover_selected_running_jobs_for_drain(drain_lock, &recoverable)
        .map_err(ShipExecutionError::from)
}

fn scheduler_host_pools(
    jobs: &[Job],
    request_store: &QueueRequestStore,
) -> Result<Vec<HostPoolConfig>, ShipExecutionError> {
    let mut pools = BTreeMap::<String, HostPoolConfig>::new();
    for job in jobs
        .iter()
        .filter(|job| matches!(job.status, JobStatus::Pending | JobStatus::Running))
    {
        let Some(envelope) = request_store.load(&job.id)? else {
            continue;
        };
        let targets = match envelope.kind {
            QueuedExecutionKind::Run => envelope.to_run_request()?.targets,
            QueuedExecutionKind::Ship => envelope.to_ship_request()?.targets,
        };
        for target in &targets {
            collect_target_host_pools(target, &mut pools);
        }
    }
    Ok(pools.into_values().collect())
}

fn collect_target_host_pools(
    target: &ResolvedTarget,
    pools: &mut BTreeMap<String, HostPoolConfig>,
) {
    match &target.backend {
        ResolvedBackend::HostPool(pool) => {
            pools
                .entry(pool.pool_name.clone())
                .or_insert_with(|| host_pool_config_from_resolved(pool));
        }
        ResolvedBackend::Fallback(chain) => {
            for backend in &chain.backends {
                collect_target_host_pools(&backend.target, pools);
            }
        }
        ResolvedBackend::Local(_)
        | ResolvedBackend::Ssh(_)
        | ResolvedBackend::Windows(_)
        | ResolvedBackend::Cloud(_) => {}
    }
}

fn host_pool_config_from_resolved(pool: &ResolvedHostPoolConfig) -> HostPoolConfig {
    HostPoolConfig {
        name: pool.pool_name.clone(),
        strategy: pool.strategy.clone(),
        lease_stale_seconds: pool.lease_stale_seconds,
        heartbeat_interval_seconds: pool.heartbeat_interval_seconds,
        members: pool
            .members
            .iter()
            .map(host_pool_member_config_from_resolved)
            .collect(),
    }
}

fn host_pool_member_config_from_resolved(member: &ResolvedHostPoolMember) -> HostPoolMemberConfig {
    match &member.target.backend {
        ResolvedBackend::Local(config) => HostPoolMemberConfig {
            id: member.id.clone(),
            backend_type: "local".to_owned(),
            host: None,
            repo_path: None,
            cwd: config.cwd.clone(),
            max_concurrency: member.max_concurrency,
            capabilities: member.capabilities.clone(),
        },
        ResolvedBackend::Ssh(config) => HostPoolMemberConfig {
            id: member.id.clone(),
            backend_type: "ssh".to_owned(),
            host: config.host.clone(),
            repo_path: Some(config.repo_path.clone()),
            cwd: None,
            max_concurrency: member.max_concurrency,
            capabilities: member.capabilities.clone(),
        },
        ResolvedBackend::Windows(config) => HostPoolMemberConfig {
            id: member.id.clone(),
            backend_type: "ssh".to_owned(),
            host: config.host.clone(),
            repo_path: Some(config.repo_path.clone()),
            cwd: None,
            max_concurrency: member.max_concurrency,
            capabilities: member.capabilities.clone(),
        },
        ResolvedBackend::Cloud(_) | ResolvedBackend::HostPool(_) | ResolvedBackend::Fallback(_) => {
            HostPoolMemberConfig {
                id: member.id.clone(),
                backend_type: member.target.backend_name.clone(),
                host: member.target.host.clone(),
                repo_path: member.target.workdir(),
                cwd: None,
                max_concurrency: member.max_concurrency,
                capabilities: member.capabilities.clone(),
            }
        }
    }
}

fn terminal_run_outcome(
    queue: &mut Queue,
    state_dir: &Path,
    job_id: &str,
) -> Result<Option<RunExecutionOutcome>, ShipExecutionError> {
    let Some(job) = queue.get(job_id)? else {
        return Ok(None);
    };
    match job.status {
        JobStatus::Cancelled => Ok(Some(RunExecutionOutcome { job })),
        JobStatus::Completed => load_run_outcome(queue, state_dir, job_id).map(Some),
        JobStatus::Pending | JobStatus::Running => Ok(None),
    }
}

fn terminal_ship_outcome(
    queue: &mut Queue,
    state_dir: &Path,
    request: &ShipExecutionRequest,
    job_id: &str,
) -> Result<Option<ShipExecutionOutcome>, ShipExecutionError> {
    let Some(job) = queue.get(job_id)? else {
        return Ok(None);
    };
    match job.status {
        JobStatus::Cancelled => {
            let loaded = QueueOutcomeStore::new(state_dir)
                .map_err(QueueRequestError::from)?
                .load(job_id)?;
            if loaded.is_some() {
                return load_ship_outcome(queue, state_dir, job_id).map(Some);
            }
            Ok(Some(ShipExecutionOutcome {
                job,
                ship_state: unsaved_ship_state(request, &target_names(&request.targets)),
                resumed_existing_state: false,
                post_validation: None,
            }))
        }
        JobStatus::Completed => load_ship_outcome(queue, state_dir, job_id).map(Some),
        JobStatus::Pending | JobStatus::Running => Ok(None),
    }
}

fn wait_or_timeout(
    job_id: &str,
    wait_iterations: &mut usize,
    options: CooperativeDrainOptions,
) -> Result<(), ShipExecutionError> {
    *wait_iterations = wait_iterations.saturating_add(1);
    if let Some(max) = options.max_wait_iterations
        && *wait_iterations > max
    {
        return Err(ShipExecutionError::CooperativeWaitTimedOut(
            job_id.to_owned(),
        ));
    }
    if !options.poll_interval.is_zero() {
        thread::sleep(options.poll_interval);
    }
    Ok(())
}

fn refuse_same_pr_running_ship(
    queue: &mut Queue,
    state_dir: &Path,
    request: &ShipExecutionRequest,
) -> Result<(), ShipExecutionError> {
    let request_store = QueueRequestStore::new(state_dir).map_err(QueueRequestError::from)?;
    let stale_after = chrono::Duration::seconds(DEFAULT_RUNNING_JOB_STALE_SECONDS);
    for running in queue.get_running()? {
        let Some(envelope) = request_store.load(&running.id)? else {
            continue;
        };
        if envelope.job_id != running.id {
            return Err(ShipExecutionError::QueueRequest(
                QueueRequestError::InvalidSnapshot {
                    reason: format!(
                        "running queued execution request for {} belongs to {}",
                        running.id, envelope.job_id
                    ),
                },
            ));
        }
        let daemon_owned = envelope.is_daemon_owned() || envelope.is_daemon_admissible();
        let QueuedExecutionEnvelope {
            request: crate::queue_request::QueuedExecutionRequest::Ship(existing),
            ..
        } = envelope
        else {
            continue;
        };
        if canonical_repository(&existing.repo) != canonical_repository(&request.repo)
            || existing.pr != request.pr
        {
            continue;
        }

        // Daemon-owned work is reconciled exclusively by the durable
        // supervisor, which can prove an exact live worker receipt. A
        // foreground submitter must never reap it on heartbeat age alone.
        if daemon_owned {
            return Err(ShipExecutionError::SamePrShipRunning {
                repo: request.repo.clone(),
                pr: request.pr,
                running_job_id: running.id,
            });
        }

        // A same-PR ship is already running. If its worker has gone silent past
        // the heartbeat-staleness threshold it was abandoned (e.g. the process
        // was killed) and must not block this retry forever — reap it and move
        // on. `cancel_stale_running_jobs` re-checks staleness under the queue
        // lock, so a worker that is merely between heartbeats is never reaped
        // out from under itself.
        let now = Utc::now();
        if running.is_stale_running(now, stale_after) {
            let reaped = queue.cancel_stale_running_jobs(
                std::slice::from_ref(&running.id),
                now,
                stale_after,
                STALE_RUNNING_CANCEL_REASON,
            )?;
            if !reaped.is_empty() {
                continue;
            }
            // The under-lock re-check disagreed — a heartbeat landed between the
            // snapshot and the reap, so the worker is live after all. Fall
            // through to refuse only if it is genuinely still running.
            match queue.get(&running.id)? {
                Some(job) if job.status == JobStatus::Running => {}
                _ => continue,
            }
        }

        return Err(ShipExecutionError::SamePrShipRunning {
            repo: request.repo.clone(),
            pr: request.pr,
            running_job_id: running.id,
        });
    }
    Ok(())
}

fn ensure_worker_running_job(queue: &mut Queue, job: &Job) -> Result<Job, ShipExecutionError> {
    match job.status {
        JobStatus::Pending => {
            let started = job.start()?;
            queue.update(&started)?;
            Ok(started)
        }
        JobStatus::Running => Ok(queue.get(&job.id)?.unwrap_or_else(|| job.clone())),
        status => Err(JobTransitionError::InvalidStart(status).into()),
    }
}

fn unsaved_ship_state(request: &ShipExecutionRequest, target_names: &[String]) -> ShipState {
    let mut state = ShipState::new(
        request.pr,
        request.repo.clone(),
        request.branch.clone(),
        request.base_branch.clone(),
        request.sha.clone(),
        policy_signature(&request.targets, target_names, request.mode),
    );
    refresh_pr_metadata(&mut state, request);
    state
}

#[allow(clippy::too_many_lines)]
// This internal execution loop threads several heterogeneous handles (stores,
/// Mostly default-off knobs for one target-execution pass. Bundled so the
/// execution seam takes a single cohesive options value rather than a growing
/// list of loose booleans and paths — and so a new opt-in is one field, not one
/// more positional argument. Resolved once at the command layer.
#[derive(Clone, Copy)]
struct TargetExecOptions<'a> {
    /// Durable submission cwd used by local targets without an explicit path.
    cwd: &'a Path,
    /// Return scheduler-owned deferral results for transient host-pool lease
    /// contention instead of final busy target results.
    defer_host_pool_lease_unavailable: bool,
    /// When set, a local `TEST` failure overlapping a host infra incident is
    /// relabeled `INFRA` (opt-in host-vitals reclassification); `None` = off.
    reclassify_vitals_path: Option<&'a Path>,
    /// Same-backend retry budget for transient local `INFRA` blips (default off).
    transient_retry: crate::ship_retry::TransientRetryPolicy,
    /// Proven worker configuration used by throttled exact-head cancellation.
    config: Option<&'a LoadedConfig>,
}

fn stale_head_reason(queued_head: &str, live_head: &str) -> Option<String> {
    let is_sha =
        |value: &str| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    (is_sha(queued_head) && is_sha(live_head) && queued_head != live_head).then(|| {
        format!(
            "PR head changed from queued {queued_head} to {live_head}; cancelling stale validation"
        )
    })
}

// The per-target retry loop keeps the cancellation-sensitive dispatch +
// progress-callback block inline (it borrows `job`/`queue` mutably and early-
// returns on Deferred/Cancelled). Extracting the per-target body into its own
// runner is the right next step, but as a separate no-behavior-change refactor
// so a cancellation regression stays bisectable — not bundled with this opt-in.
#[allow(clippy::too_many_lines)]
fn execute_targets_with_options<D: ShipTargetDispatcher>(
    request: &ShipExecutionRequest,
    state_dir: &Path,
    queue: &mut Queue,
    warm_pool: &WarmPool,
    dispatcher: &D,
    mut job: Job,
    options: TargetExecOptions<'_>,
) -> Result<TargetExecutionOutcome, ShipExecutionError> {
    let mut had_failure = false;
    for target in &request.targets {
        if let Some(cancelled) = durable_cancelled_job(queue, &job)? {
            return Ok(TargetExecutionOutcome::Completed(cancelled));
        }
        if had_failure && request.fail_fast {
            job = job.with_result(cancelled_result(target, job.started_at));
            queue.update(&job)?;
            continue;
        }
        let base_log_path = target_log_path(state_dir, &job.id, &target.name);
        let execution_target = target.clone().with_default_local_workdir(options.cwd);
        let decision = apply_warm_reuse(
            warm_pool,
            &execution_target,
            &request.sha,
            request.resume_from.as_deref(),
            request.warm_disabled,
            crate::warm_pool::now_epoch_secs(),
        );

        // Same-backend retry loop for transient local INFRA blips. When the
        // policy is disabled (the default) this runs exactly once against the
        // base log path — byte-identical to the non-retry behavior.
        let mut attempt: u32 = 0;
        let mut prior_transient: Vec<String> = Vec::new();
        let mut last_head_check: Option<Instant> = None;
        let result = loop {
            let attempt_log_path = retry_attempt_log_path(&base_log_path, attempt);
            job = job.with_result(running_result(
                &decision.target,
                &attempt_log_path,
                job.started_at,
            ));
            queue.update(&job)?;

            let dispatch_job_id = job.id.clone();
            let progress_log_path = attempt_log_path.clone();
            let mut progress_error = None;
            let mut progress_cancelled = None;
            let mut result = {
                let mut progress_callback = |event: ProgressEvent| {
                    if progress_error.is_some() || progress_cancelled.is_some() {
                        return ProgressAction::Terminate(
                            "durable queue cancellation or progress persistence failure".to_owned(),
                        );
                    }
                    match durable_cancelled_job(queue, &job) {
                        Ok(Some(cancelled)) => {
                            let reason =
                                cancelled.cancellation_reason.clone().unwrap_or_else(|| {
                                    "durable queue cancellation requested".to_owned()
                                });
                            progress_cancelled = Some(cancelled);
                            return ProgressAction::Terminate(reason);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let reason = error.to_string();
                            progress_error = Some(error);
                            return ProgressAction::Terminate(reason);
                        }
                    }
                    if let Some(config) = options.config
                        && request.pr != 0
                        && !request.repo.is_empty()
                        && last_head_check
                            .is_none_or(|last| last.elapsed() >= Duration::from_mins(1))
                    {
                        last_head_check = Some(Instant::now());
                        if let Ok((live_head, _)) =
                            crate::reconcile::fetch_head_and_status_check_rollup_with_config(
                                config,
                                options.cwd,
                                &request.repo,
                                request.pr,
                            )
                            && let Some(reason) = stale_head_reason(&request.sha, &live_head)
                        {
                            match queue.request_cancel(&job.id, Some(reason.clone())) {
                                Ok(Some(cancelled)) => {
                                    progress_cancelled = Some(cancelled);
                                }
                                Ok(None) => {
                                    progress_error =
                                        Some(ShipExecutionError::MissingQueuedJob(job.id.clone()));
                                }
                                Err(error) => {
                                    progress_error = Some(ShipExecutionError::Queue(error));
                                }
                            }
                            return ProgressAction::Terminate(reason);
                        }
                    }
                    apply_progress_event(&mut job, &decision.target, &progress_log_path, event);
                    if let Err(error) = queue.update(&job) {
                        let reason = error.to_string();
                        progress_error = Some(ShipExecutionError::Queue(error));
                        return ProgressAction::Terminate(reason);
                    }
                    match durable_cancelled_job(queue, &job) {
                        Ok(Some(cancelled)) => {
                            let reason =
                                cancelled.cancellation_reason.clone().unwrap_or_else(|| {
                                    "durable queue cancellation requested".to_owned()
                                });
                            progress_cancelled = Some(cancelled);
                            ProgressAction::Terminate(reason)
                        }
                        Ok(None) => ProgressAction::Continue,
                        Err(error) => {
                            let reason = error.to_string();
                            progress_error = Some(error);
                            ProgressAction::Terminate(reason)
                        }
                    }
                };
                dispatcher.validate(DispatchValidationRequest {
                    job_id: Some(dispatch_job_id),
                    defer_host_pool_lease_unavailable: options.defer_host_pool_lease_unavailable,
                    sha: request.sha.clone(),
                    branch: request.branch.clone(),
                    target: &decision.target,
                    log_path: attempt_log_path,
                    resume_from: decision.resume_from.clone(),
                    mode: request.mode,
                    progress_callback: Some(&mut progress_callback),
                })
            };
            if let Some(error) = progress_error {
                return Err(error);
            }
            if let Some(cancelled) = progress_cancelled {
                return Ok(TargetExecutionOutcome::Completed(cancelled));
            }
            if let Some(cancelled) = durable_cancelled_job(queue, &job)? {
                return Ok(TargetExecutionOutcome::Completed(cancelled));
            }
            // A scheduler-deferred result is never terminal — it must not be
            // retried or persisted as a failure; hand it back on the defer path.
            if result.is_scheduler_deferred() {
                let reason = result
                    .scheduler_defer_reason
                    .clone()
                    .unwrap_or_else(|| "scheduler_deferred".to_owned());
                return Ok(TargetExecutionOutcome::Deferred { job, reason });
            }
            maybe_reclassify_on_host_incident(&mut result, options.reclassify_vitals_path);

            // Re-run only a transient local INFRA blip, bounded by the policy.
            let should_retry = attempt < options.transient_retry.max_retries()
                && !result.passed()
                && result.backend == "local"
                && crate::classify::same_leg_local_retryable(result.failure_class.as_deref());
            if !should_retry {
                break annotate_retry_history(result, &prior_transient);
            }

            // A cancel may have landed while this attempt ran; honor it before
            // spending another attempt.
            if let Some(cancelled) = durable_cancelled_job(queue, &job)? {
                return Ok(TargetExecutionOutcome::Completed(cancelled));
            }
            prior_transient.push(format!(
                "attempt {}: {} (log {})",
                attempt + 1,
                result.failure_class.as_deref().unwrap_or("INFRA"),
                progress_log_path.display()
            ));
            attempt += 1;
        };

        job = job.with_result(result.clone());
        queue.update(&job)?;
        if !result.passed() {
            had_failure = true;
        }
        update_warm_pool_after_run(
            warm_pool,
            &decision.target,
            &request.sha,
            &result,
            decision.warm_hit,
            request.warm_disabled,
            crate::warm_pool::now_epoch_secs(),
        )
        .map_err(ShipExecutionError::WarmPool)?;
    }
    Ok(TargetExecutionOutcome::Completed(job))
}

/// Opt-in, fail-open host-incident reclassification. When enabled (a resolved
/// vitals path is present), a LOCAL leg that failed with a plain `TEST` class is
/// relabeled `INFRA` — with an honest note — if a host infra incident (jetsam /
/// `WindowServer` crash) overlapped its window, so the author isn't misled into
/// debugging their own code after the host shed load under them. Purely a label
/// plus note: it never changes `TargetStatus`, so merge readiness (which keys on
/// status, not `failure_class`) is unaffected. No-op for remote/cloud legs, any
/// non-`TEST` class, or an absent/stale signal.
fn maybe_reclassify_on_host_incident(
    result: &mut TargetResult,
    reclassify_vitals_path: Option<&Path>,
) {
    let Some(path) = reclassify_vitals_path else {
        return;
    };
    if result.passed() || result.backend != "local" {
        return;
    }
    let (Some(started_at), Some(completed_at)) = (result.started_at, result.completed_at) else {
        return;
    };
    // Cheap class-eligibility check before the filesystem probe.
    let Some(new_class) = crate::classify::promote_test_to_infra(result.failure_class.as_deref())
    else {
        return;
    };
    let Some(reason) = crate::host_health::incident_from_path(path, started_at, completed_at)
    else {
        return;
    };
    result.failure_class = Some(new_class.as_str().to_owned());
    let note = format!("Reclassified to {new_class} — {reason}");
    result.error_message = Some(match result.error_message.take() {
        Some(existing) if !existing.is_empty() => format!("{existing}\n{note}"),
        _ => note,
    });
}

fn apply_progress_event(
    job: &mut Job,
    target: &ResolvedTarget,
    log_path: &Path,
    event: ProgressEvent,
) {
    let mut current = job
        .results
        .get(&target.name)
        .cloned()
        .unwrap_or_else(|| running_result(target, log_path, job.started_at));
    current.status = TargetStatus::Running;
    if let Some(phase) = event.phase {
        current.phase = Some(phase);
    }
    if let Some(last_output_at) = event.last_output_at {
        current.last_output_at = Some(last_output_at);
    }
    current.last_heartbeat_at = Some(event.last_heartbeat_at);
    current.quiet_for_secs = Some(event.quiet_for_secs);
    current.liveness = Some(event.liveness);
    current.log_path = current
        .log_path
        .or_else(|| Some(log_path.to_string_lossy().into_owned()));
    *job = job.with_result(current);
}

fn load_or_create_state(
    request: &ShipExecutionRequest,
    target_names: &[String],
    store: &ShipStateStore,
    lock: Option<&ShipStatePrLock>,
) -> Result<ShipState, ShipExecutionError> {
    let policy = policy_signature(&request.targets, target_names, request.mode);
    let existing = lock.map_or_else(
        || store.get_scoped(&request.repo, request.pr),
        |lock| store.get_locked_scoped(&request.repo, request.pr, lock),
    );
    if let Some(mut existing) = existing {
        validate_existing_state(
            &existing,
            &request.sha,
            &request.base_branch,
            &policy,
            request.adopt_head,
        )?;
        let validation_identity_drift =
            existing.is_sha_drift(&request.sha) || existing.base_branch != request.base_branch;
        if request.adopt_head && validation_identity_drift {
            // Adopt the amended/force-pushed head or retargeted base. Clear
            // prior remote runs and evidence so the new validation identity is
            // re-validated from scratch — never bless stale validation for a
            // different tree or merge target. `head_sha` and `base_branch`
            // also gate auto-merge's live preflight, so both must track what
            // this execution actually validates (Shipyard #346).
            existing.head_sha.clone_from(&request.sha);
            existing.base_branch.clone_from(&request.base_branch);
            existing.dispatched_runs.clear();
            existing.evidence_snapshot.clear();
            existing.merge_queue_observed_at = None;
            // Establish a fresh authority epoch for the adopted head. This is
            // deliberately not ownership by itself; it only prevents removal
            // events from the previous SHA from governing the new validation.
            existing.merge_queue_attempt_started_at = Some(chrono::Utc::now());
            existing.merge_queue_enqueue_succeeded_at = None;
            existing.merge_queue_enqueue_started_at = None;
        }
        existing.commit_subject.clone_from(&request.commit_subject);
        refresh_pr_metadata(&mut existing, request);
        // Beginning a ship execution is the intended recovery from an opt-in
        // orphan abandonment: clear the terminal marker so a re-shipped PR is
        // no longer short-circuited to failure by `ship_terminal_verdict`. A
        // non-abandoned state already has `None` here, so this is a no-op then.
        existing.abandoned = None;
        existing.touch();
        return Ok(existing);
    }

    let mut state = ShipState::new(
        request.pr,
        request.repo.clone(),
        request.branch.clone(),
        request.base_branch.clone(),
        request.sha.clone(),
        policy,
    );
    refresh_pr_metadata(&mut state, request);
    if state.pr_url.is_empty() && !request.repo.is_empty() {
        state.pr_url = format!("https://github.com/{}/pull/{}", request.repo, request.pr);
    }
    state.commit_subject.clone_from(&request.commit_subject);
    Ok(state)
}

fn refresh_pr_metadata(state: &mut ShipState, request: &ShipExecutionRequest) {
    if let Some(pr_url) = request.pr_url.as_deref().filter(|value| !value.is_empty()) {
        pr_url.clone_into(&mut state.pr_url);
    }
    if let Some(pr_title) = request
        .pr_title
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        pr_title.clone_into(&mut state.pr_title);
    }
}

fn validate_existing_state(
    state: &ShipState,
    sha: &str,
    base_branch: &str,
    policy: &str,
    adopt_head: bool,
) -> Result<(), ShipExecutionError> {
    if !adopt_head && state.is_sha_drift(sha) {
        return Err(ShipExecutionError::ShaDrift {
            existing: state.head_sha.clone(),
            current: sha.to_owned(),
        });
    }
    if !adopt_head && state.base_branch != base_branch {
        return Err(ShipExecutionError::BaseDrift {
            existing: state.base_branch.clone(),
            current: base_branch.to_owned(),
        });
    }
    if state.policy_signature != policy {
        return Err(ShipExecutionError::PolicyDrift {
            existing: state.policy_signature.clone(),
            current: policy.to_owned(),
        });
    }
    Ok(())
}

fn target_names(targets: &[ResolvedTarget]) -> Vec<String> {
    targets
        .iter()
        .map(|target| target.name.clone())
        .collect::<Vec<_>>()
}

fn target_log_path(state_dir: &Path, job_id: &str, target: &str) -> PathBuf {
    state_dir
        .join("logs")
        .join(job_id)
        .join(format!("{target}.log"))
}

fn running_result(
    target: &ResolvedTarget,
    log_path: &Path,
    started_at: Option<chrono::DateTime<Utc>>,
) -> TargetResult {
    let mut result = TargetResult::new(
        target.name.clone(),
        target.platform.clone(),
        TargetStatus::Running,
        target.backend_name.clone(),
    );
    result.started_at = started_at;
    result.log_path = Some(log_path.to_string_lossy().into_owned());
    result
}

fn cancelled_result(
    target: &ResolvedTarget,
    started_at: Option<chrono::DateTime<Utc>>,
) -> TargetResult {
    let mut result = TargetResult::new(
        target.name.clone(),
        target.platform.clone(),
        TargetStatus::Cancelled,
        "skipped",
    );
    result.started_at = started_at;
    result.completed_at = Some(Utc::now());
    result.error_message = Some("Skipped (earlier target failed, --fail-fast)".to_owned());
    result
}

/// Log path for a given same-backend retry attempt of a target. Attempt 0 uses
/// the stable base path, so a run with retries disabled is byte-identical to
/// before. Each retry writes to a distinct `.retry<N>` sibling, so re-running
/// never truncates the failing attempt's log — that preserved evidence is
/// exactly what makes a transient-vs-real determination possible after the fact.
/// Distinct from the dispatch layer's `.attempt-<N>` cross-backend failover logs.
fn retry_attempt_log_path(base: &Path, attempt: u32) -> PathBuf {
    if attempt == 0 {
        return base.to_path_buf();
    }
    let mut file_name = base
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    file_name.push(format!(".retry{attempt}"));
    base.with_file_name(file_name)
}

/// Fold same-backend retry history into the terminal result so the outcome is
/// honest about the re-runs. No retries → returned unchanged. A recovered
/// (passed) result records the recovery in `phase` and never in `error_message`
/// — a non-empty `error_message` is read elsewhere as a failure signal. A result
/// that is still failing appends the transient history to `error_message`,
/// matching the incident-reclassification note style.
fn annotate_retry_history(mut result: TargetResult, prior_transient: &[String]) -> TargetResult {
    if prior_transient.is_empty() {
        return result;
    }
    let retries = prior_transient.len();
    let plural = if retries == 1 { "y" } else { "ies" };
    let history = prior_transient.join("; ");
    if result.passed() {
        result.phase = Some(format!(
            "recovered after {retries} same-backend transient retr{plural} ({history})"
        ));
    } else {
        let note = format!(
            "Same-backend transient retry exhausted after {retries} retr{plural} ({history})."
        );
        result.error_message = Some(match result.error_message.take() {
            Some(existing) if !existing.is_empty() => format!("{existing}\n{note}"),
            _ => note,
        });
    }
    result
}

fn record_evidence(
    evidence: &EvidenceStore,
    workload_scope: &str,
    request: &ShipExecutionRequest,
    job: &Job,
) -> Result<(), ShipExecutionError> {
    let targets = request
        .targets
        .iter()
        .map(|target| (target.name.as_str(), target))
        .collect::<BTreeMap<_, _>>();
    for result in job.results.values() {
        let target = targets.get(result.target_name.as_str()).copied();
        evidence
            .record_scoped(workload_scope, &evidence_record(request, result, target))
            .map_err(|error| ShipExecutionError::Evidence(error.to_string()))?;
    }
    Ok(())
}

fn evidence_record(
    request: &ShipExecutionRequest,
    result: &TargetResult,
    target: Option<&ResolvedTarget>,
) -> EvidenceRecord {
    EvidenceRecord {
        sha: request.sha.clone(),
        branch: request.branch.clone(),
        workload_scope: None,
        target_name: result.target_name.clone(),
        validation_build_type: target.and_then(|target| target.validation_build_type.clone()),
        platform: result.platform.clone(),
        status: evidence_status(result).to_owned(),
        backend: result.backend.clone(),
        source_head_sha: result.source_head_sha.clone(),
        source_tree_sha: result.source_tree_sha.clone(),
        source_checkout_clean: result.source_checkout_clean,
        full_execution: result.full_execution,
        completed_at: result.completed_at.unwrap_or_else(Utc::now),
        duration_secs: result.duration_secs,
        host: target.and_then(|target| target.host.clone()),
        primary_backend: result.primary_backend.clone(),
        failover_reason: result.failover_reason.clone(),
        provider: result.provider.clone(),
        runner_profile: result.runner_profile.clone(),
        failure_class: result.failure_class.clone(),
        reused_from: result.reused_from.clone(),
        contract_digest: target.and_then(crate::queue_request::validation_contract_digest),
        stages_signature: None,
    }
}

fn update_ship_state_from_job(state: &mut ShipState, request: &ShipExecutionRequest, job: &Job) {
    let targets = request
        .targets
        .iter()
        .map(|target| (target.name.as_str(), target))
        .collect::<BTreeMap<_, _>>();
    for result in job.results.values() {
        state.update_evidence(&result.target_name, evidence_status(result));
        let run = dispatched_run(
            state,
            job,
            result,
            targets.get(result.target_name.as_str()).copied(),
            !request.advisory_targets.contains(&result.target_name),
        );
        state.upsert_run(run);
    }
}

fn dispatched_run(
    state: &ShipState,
    job: &Job,
    result: &TargetResult,
    target: Option<&ResolvedTarget>,
    required: bool,
) -> DispatchedRun {
    let now = Utc::now();
    DispatchedRun {
        target: result.target_name.clone(),
        provider: result
            .provider
            .clone()
            .or_else(|| result.primary_backend.clone())
            .unwrap_or_else(|| {
                target.map_or_else(
                    || result.backend.clone(),
                    |target| target.backend_name.clone(),
                )
            }),
        // Issue #303: prefer the cloud (GHA) workflow run id when present so
        // the dispatched-run record actually points at the workflow run a user
        // can open in the browser. Fall back to the internal Shipyard job id
        // for local/SSH/Windows backends that don't yield a GHA run.
        run_id: result
            .cloud_run_id
            .map_or_else(|| job.id.clone(), |id| id.to_string()),
        status: if result.passed() {
            "completed".to_owned()
        } else {
            "failed".to_owned()
        },
        started_at: result.started_at.unwrap_or(now),
        updated_at: result.completed_at.unwrap_or(now),
        attempt: state.attempt,
        last_heartbeat_at: result.last_heartbeat_at,
        phase: result.phase.clone(),
        required,
    }
}

fn evidence_status(result: &TargetResult) -> &'static str {
    if result.passed() { "pass" } else { "fail" }
}

/// Result of consulting the warm pool for a target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WarmReuseDecision {
    /// Target after any warm workdir override has been applied.
    pub target: ResolvedTarget,
    /// Stable pool key for this target/host pair.
    pub host_key: String,
    /// Whether a live same-SHA pool entry was consumed.
    pub warm_hit: bool,
    /// Effective resume stage for the executor.
    pub resume_from: Option<String>,
}

/// Mutation performed after a target run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarmPoolUpdate {
    /// No pool mutation was needed.
    Noop,
    /// A passing target refreshed or inserted an entry.
    Upserted,
    /// A failing warm reuse evicted an entry.
    Evicted,
}

/// Apply same-SHA warm-pool reuse to a resolved target when possible.
#[must_use]
pub fn apply_warm_reuse(
    pool: &WarmPool,
    target: &ResolvedTarget,
    sha: &str,
    requested_resume_from: Option<&str>,
    globally_off: bool,
    now: f64,
) -> WarmReuseDecision {
    let host_key = warm_host_key(target.host.as_deref());
    let resume_from = requested_resume_from.map(ToOwned::to_owned);
    if globally_off
        || target.warm_keepalive_seconds == 0
        || !is_backend_eligible(&target.backend_name)
    {
        return miss(target, host_key, resume_from);
    }

    let Some(entry) = pool.get(&target.name, &host_key, now) else {
        return miss(target, host_key, resume_from);
    };
    if entry.sha != sha {
        return miss(target, host_key, resume_from);
    }

    WarmReuseDecision {
        target: target.clone().with_workdir(entry.workdir),
        host_key,
        warm_hit: true,
        resume_from: Some(effective_warm_resume(requested_resume_from).to_owned()),
    }
}

/// Record or evict a warm-pool entry after a target run.
pub fn update_warm_pool_after_run(
    pool: &WarmPool,
    target: &ResolvedTarget,
    sha: &str,
    result: &TargetResult,
    warm_was_applied: bool,
    globally_off: bool,
    now: f64,
) -> Result<WarmPoolUpdate, std::io::Error> {
    let host = warm_host_key(target.host.as_deref());
    if result.passed() {
        if globally_off
            || target.warm_keepalive_seconds == 0
            || !is_backend_eligible(&target.backend_name)
        {
            return Ok(WarmPoolUpdate::Noop);
        }
        pool.upsert(PoolEntry::new(
            target.name.clone(),
            host,
            target.backend_name.clone(),
            target
                .workdir()
                .unwrap_or_else(|| DEFAULT_WORKDIR.to_owned()),
            sha.to_owned(),
            compute_expires_at(target.warm_keepalive_seconds, now),
            now,
        ))?;
        return Ok(WarmPoolUpdate::Upserted);
    }

    if warm_was_applied {
        let _removed = pool.evict(&target.name, &host)?;
        return Ok(WarmPoolUpdate::Evicted);
    }
    Ok(WarmPoolUpdate::Noop)
}

fn miss(
    target: &ResolvedTarget,
    host_key: String,
    resume_from: Option<String>,
) -> WarmReuseDecision {
    WarmReuseDecision {
        target: target.clone(),
        host_key,
        warm_hit: false,
        resume_from,
    }
}

fn effective_warm_resume(requested_resume_from: Option<&str>) -> &str {
    let Some(requested) = requested_resume_from else {
        return WARM_DEFAULT_RESUME_FROM;
    };
    let Some(requested_index) = RESUME_ORDER.iter().position(|stage| *stage == requested) else {
        return WARM_DEFAULT_RESUME_FROM;
    };
    let default_index = RESUME_ORDER
        .iter()
        .position(|stage| *stage == WARM_DEFAULT_RESUME_FROM)
        .expect("warm default is in order");
    if requested_index > default_index {
        requested
    } else {
        WARM_DEFAULT_RESUME_FROM
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::{Barrier, Condvar, Mutex};
    use std::time::Duration as StdDuration;

    use chrono::{DateTime, Duration, Utc};
    use toml::Table;

    use super::{
        CooperativeDrainOptions, QueueRequestError, QueuedExecutionOwner, RunExecutionRequest,
        RunStores, RuntimeMode, ShipExecutionError, ShipExecutionRequest, ShipStores,
        ShipTargetDispatcher, TargetExecOptions, TargetExecutionOutcome, WarmPoolUpdate,
        apply_warm_reuse, cap_admit_pass_workers, drain_or_wait_run,
        drain_or_wait_run_with_options, drain_or_wait_ship_awaited_only,
        drain_or_wait_ship_with_scope, execute_run, execute_run_worker, execute_ship,
        execute_ship_worker, execute_targets_with_options,
        leases_not_covered_by_running_reservations, load_run_outcome, load_ship_outcome,
        recover_foreground_running_jobs_for_drain, retry_attempt_log_path, run_drain_worker_cycle,
        stale_head_reason, submit_run, submit_ship, target_log_path, update_warm_pool_after_run,
    };
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use crate::evidence::EvidenceStore;
    use crate::executor::dispatch::{
        DispatchValidationRequest, ResolvedTarget, resolve_targets_from_table,
    };
    use crate::executor::streaming::{ProgressAction, ProgressEvent};
    use crate::host_pool::HostPoolLease;
    use crate::job::{Job, JobStatus, Priority, TargetResult, TargetStatus, ValidationMode};
    use crate::queue::Queue;
    use crate::queue_request::{
        QueueOutcomeStore, QueueRequestStore, QueuedExecutionOutcome, QueuedExecutionRequest,
    };
    use crate::queue_scheduler::{AdmitPassPlan, RequestBackedAdmitPass, SamePrShipAdmission};
    use crate::ship_state::{AbandonRecord, ShipState, ShipStateStore};
    use crate::warm_pool::{PoolEntry, WarmPool};

    #[test]
    fn exact_head_monitor_only_cancels_real_sha_drift() {
        let queued = "a".repeat(40);
        let live = "b".repeat(40);
        assert_eq!(stale_head_reason(&queued, &queued), None);
        assert_eq!(stale_head_reason(&queued, ""), None);
        assert_eq!(stale_head_reason(&queued, "malformed"), None);
        assert_eq!(
            stale_head_reason(&queued, &live),
            Some(format!(
                "PR head changed from queued {queued} to {live}; cancelling stale validation"
            ))
        );
    }

    fn table(input: &str) -> Table {
        input.parse::<Table>().expect("valid TOML")
    }

    // ---- host-incident reclassification seam (Part 2) ----

    fn failed_local_test_result(started: DateTime<Utc>, completed: DateTime<Utc>) -> TargetResult {
        let mut result = TargetResult::new("mac", "macos-arm64", TargetStatus::Fail, "local");
        result.started_at = Some(started);
        result.completed_at = Some(completed);
        result.failure_class = Some("TEST".to_owned());
        result
    }

    fn write_host_vitals(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("host_vitals.json");
        std::fs::write(&path, body).expect("write vitals");
        path
    }

    #[test]
    fn reclassify_promotes_local_test_to_infra_on_overlapping_jetsam() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_host_vitals(dir.path(), r#"{"jetsam_age_s":5}"#);
        let now = Utc::now();
        let mut result =
            failed_local_test_result(now - Duration::hours(1), now + Duration::hours(1));
        super::maybe_reclassify_on_host_incident(&mut result, Some(&path));
        assert_eq!(result.failure_class.as_deref(), Some("INFRA"));
        assert!(
            result.error_message.unwrap_or_default().contains("jetsam"),
            "reclassification should note the reason"
        );
    }

    #[test]
    fn reclassify_is_noop_without_a_resolved_path() {
        let now = Utc::now();
        let mut result =
            failed_local_test_result(now - Duration::hours(1), now + Duration::hours(1));
        super::maybe_reclassify_on_host_incident(&mut result, None);
        assert_eq!(result.failure_class.as_deref(), Some("TEST"));
    }

    #[test]
    fn reclassify_skips_remote_backends() {
        // SSH/cloud legs run on another host; local DiagnosticReports don't apply.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_host_vitals(dir.path(), r#"{"jetsam_age_s":5}"#);
        let now = Utc::now();
        let mut result =
            failed_local_test_result(now - Duration::hours(1), now + Duration::hours(1));
        result.backend = "ssh".to_owned();
        super::maybe_reclassify_on_host_incident(&mut result, Some(&path));
        assert_eq!(result.failure_class.as_deref(), Some("TEST"));
    }

    #[test]
    fn reclassify_never_masks_a_contract_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_host_vitals(dir.path(), r#"{"jetsam_age_s":5}"#);
        let now = Utc::now();
        let mut result =
            failed_local_test_result(now - Duration::hours(1), now + Duration::hours(1));
        result.failure_class = Some("CONTRACT".to_owned());
        super::maybe_reclassify_on_host_incident(&mut result, Some(&path));
        assert_eq!(result.failure_class.as_deref(), Some("CONTRACT"));
    }

    #[test]
    fn reclassify_skips_when_no_incident_overlaps_the_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Jetsam ~28h ago → far before a recent leg window.
        let path = write_host_vitals(dir.path(), r#"{"jetsam_age_s":100000}"#);
        let now = Utc::now();
        let mut result = failed_local_test_result(now - Duration::seconds(10), now);
        super::maybe_reclassify_on_host_incident(&mut result, Some(&path));
        assert_eq!(result.failure_class.as_deref(), Some("TEST"));
    }

    #[test]
    fn reclassify_preserves_the_original_failure_message() {
        // The real test-failure message must survive — the infra note is appended,
        // not substituted, so evidence of what actually failed isn't lost.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_host_vitals(dir.path(), r#"{"jetsam_age_s":5}"#);
        let now = Utc::now();
        let mut result =
            failed_local_test_result(now - Duration::hours(1), now + Duration::hours(1));
        result.error_message = Some("assertion failed: foo == bar".to_owned());
        super::maybe_reclassify_on_host_incident(&mut result, Some(&path));
        assert_eq!(result.failure_class.as_deref(), Some("INFRA"));
        let message = result.error_message.expect("message");
        assert!(
            message.contains("assertion failed: foo == bar"),
            "original preserved: {message}"
        );
        assert!(message.contains("jetsam"), "note appended: {message}");
    }

    #[test]
    fn reclassify_skips_when_timestamps_are_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_host_vitals(dir.path(), r#"{"jetsam_age_s":5}"#);
        let mut result = TargetResult::new("mac", "macos-arm64", TargetStatus::Fail, "local");
        result.failure_class = Some("TEST".to_owned());
        // started_at / completed_at left None → cannot bound a window, so no-op.
        super::maybe_reclassify_on_host_incident(&mut result, Some(&path));
        assert_eq!(result.failure_class.as_deref(), Some("TEST"));
    }

    #[test]
    fn reclassify_skips_a_passed_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_host_vitals(dir.path(), r#"{"jetsam_age_s":5}"#);
        let now = Utc::now();
        let mut result = TargetResult::new("mac", "macos-arm64", TargetStatus::Pass, "local");
        result.started_at = Some(now - Duration::hours(1));
        result.completed_at = Some(now + Duration::hours(1));
        super::maybe_reclassify_on_host_incident(&mut result, Some(&path));
        assert_eq!(result.failure_class, None);
    }

    fn empty_config(root: &std::path::Path) -> LoadedConfig {
        LoadedConfig {
            data: Table::new(),
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn macos_zero_capacity_config(root: &std::path::Path) -> LoadedConfig {
        LoadedConfig {
            data: table(
                r#"
                [host_class.studio]
                cap = 0
                tart_bin = "/bin/echo"
                "#,
            ),
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn ssh_target() -> ResolvedTarget {
        let config = table(
            r#"
            [targets.ubuntu]
            backend = "ssh"
            platform = "linux-x64"
            host = "vm"
            repo_path = "~/repo"
            warm_keepalive_seconds = 600
            "#,
        );
        resolve_targets_from_table(&config, ValidationMode::Full)
            .expect("targets")
            .remove(0)
    }

    fn local_target(name: &str, platform: &str, cwd: &std::path::Path) -> ResolvedTarget {
        let cwd = cwd
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let config = format!(
            r#"
            [validation.default]
            command = "true"

            [targets.{name}]
            backend = "local"
            platform = "{platform}"
            cwd = "{cwd}"
            "#
        )
        .parse::<Table>()
        .expect("config");
        resolve_targets_from_table(&config, ValidationMode::Full)
            .expect("targets")
            .remove(0)
    }

    fn local_target_without_cwd(name: &str, platform: &str) -> ResolvedTarget {
        let config = format!(
            r#"
            [validation.default]
            command = "true"

            [targets.{name}]
            backend = "local"
            platform = "{platform}"
            "#
        )
        .parse::<Table>()
        .expect("config");
        resolve_targets_from_table(&config, ValidationMode::Full)
            .expect("targets")
            .remove(0)
    }

    fn ship_request(targets: Vec<ResolvedTarget>) -> ShipExecutionRequest {
        ShipExecutionRequest {
            pr: 42,
            repo: "danielraffel/pulp".to_owned(),
            branch: "feature/test".to_owned(),
            base_branch: "main".to_owned(),
            sha: "abc".to_owned(),
            commit_subject: "test commit".to_owned(),
            pr_url: Some("https://github.com/danielraffel/pulp/pull/42".to_owned()),
            pr_title: Some("Test PR".to_owned()),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            advisory_targets: BTreeSet::new(),
            adopt_head: false,
            pr_snapshot_file: None,
            targets,
        }
    }

    #[test]
    fn awaited_only_ship_progresses_without_touching_unrelated_product_jobs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(state_dir.join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(state_dir.join("ship")).expect("ship state");
        let warm_pool = WarmPool::new(state_dir.join("warm_pool.json"));
        let config = empty_config(temp.path());
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);
        let target = local_target_without_cwd("mac", "macos-arm64");
        let request = |repo: &str, pr: u64, branch: &str, sha: &str| {
            let mut request = ship_request(vec![target.clone()]);
            request.repo = repo.to_owned();
            request.pr = pr;
            request.branch = branch.to_owned();
            request.sha = sha.to_owned();
            request
        };

        let unrelated_specs = [
            ("Generous-Corp/pulp", 7718, "feature/pulp", "pulp-sha"),
            (
                "Generous-Corp/forge",
                128,
                "feature/forge-sequencer",
                "sequencer-sha",
            ),
            ("Generous-Corp/vellum", 96, "feature/vellum", "vellum-sha"),
        ];
        let mut untouched = Vec::new();
        for (repo, pr, branch, sha) in unrelated_specs {
            let cwd = temp.path().join(format!("unrelated-{pr}"));
            std::fs::create_dir_all(&cwd).expect("unrelated cwd");
            let job = submit_ship(
                &request(repo, pr, branch, sha),
                &mut queue,
                &cwd,
                &state_dir,
            )
            .expect("submit unrelated ship");
            untouched.push(job);
        }

        let awaited_cwd = temp.path().join("forge-modular");
        std::fs::create_dir_all(&awaited_cwd).expect("awaited cwd");
        let awaited_request = request(
            "Generous-Corp/forge",
            127,
            "feature/forge-modular",
            "modular-sha",
        );
        let awaited_job = submit_ship(&awaited_request, &mut queue, &awaited_cwd, &state_dir)
            .expect("submit awaited ship");

        let outcome = drain_or_wait_ship_awaited_only(
            &awaited_request,
            awaited_job,
            ShipStores {
                queue: &mut queue,
                evidence: &evidence,
                ship_state: &ship_state,
                warm_pool: &warm_pool,
                cwd: &awaited_cwd,
                state_dir: &state_dir,
                config: &config,
            },
            &dispatcher,
        )
        .expect("awaited-only drain");

        assert_eq!(outcome.job.status, JobStatus::Completed);
        assert_eq!(dispatcher.seen_count(), 1);
        for original in untouched {
            assert_eq!(
                queue.get(&original.id).expect("queue").expect("job"),
                original,
                "awaited-only draining must not mutate unrelated Pulp, Forge Sequencer, or Vellum jobs"
            );
        }
    }

    #[test]
    fn awaited_only_ship_preserves_unrelated_stale_runner_claims_without_reaping_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(state_dir.join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(state_dir.join("ship")).expect("ship state");
        let warm_pool = WarmPool::new(state_dir.join("warm_pool.json"));
        let config = empty_config(temp.path());
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);
        let target = local_target_without_cwd("mac", "macos-arm64");

        let mut unrelated_request = ship_request(vec![target.clone()]);
        unrelated_request.repo = "Generous-Corp/pulp".to_owned();
        unrelated_request.pr = 7718;
        unrelated_request.branch = "feature/shared".to_owned();
        unrelated_request.sha = "old-sha".to_owned();
        let unrelated = submit_ship(&unrelated_request, &mut queue, &cwd, &state_dir)
            .expect("submit unrelated");
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");
        let mut stale = queue
            .start_pending_jobs_for_drain(&drain_lock, std::slice::from_ref(&unrelated.id))
            .expect("start unrelated")
            .pop()
            .expect("started unrelated");
        stale.started_at = Some(Utc::now() - Duration::minutes(10));
        queue.update(&stale).expect("persist stale running job");
        drop(drain_lock);

        let mut awaited_request = ship_request(vec![target]);
        awaited_request.repo = "Generous-Corp/pulp".to_owned();
        awaited_request.pr = 7719;
        awaited_request.branch = "feature/shared".to_owned();
        awaited_request.sha = "new-sha".to_owned();
        let awaited =
            submit_ship(&awaited_request, &mut queue, &cwd, &state_dir).expect("submit awaited");

        let error = drain_or_wait_ship_with_scope(
            &awaited_request,
            awaited.clone(),
            ShipStores {
                queue: &mut queue,
                evidence: &evidence,
                ship_state: &ship_state,
                warm_pool: &warm_pool,
                cwd: &cwd,
                state_dir: &state_dir,
                config: &config,
            },
            &dispatcher,
            CooperativeDrainOptions {
                poll_interval: StdDuration::ZERO,
                max_wait_iterations: Some(1),
            },
            super::DrainScope::AwaitedOnly,
        )
        .expect_err("unrelated running claim must keep awaited work deferred");

        assert!(matches!(
            error,
            ShipExecutionError::CooperativeWaitTimedOut(_)
        ));
        assert_eq!(dispatcher.seen_count(), 0);
        assert_eq!(
            queue.get(&unrelated.id).expect("queue").expect("job"),
            stale
        );
        assert_eq!(
            queue.get(&awaited.id).expect("queue").expect("job").status,
            JobStatus::Pending
        );
    }

    fn pool_with(entry: PoolEntry) -> (tempfile::TempDir, WarmPool) {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = WarmPool::new(temp.path().join("warm_pool.json"));
        pool.upsert(entry).expect("upsert");
        (temp, pool)
    }

    fn entry(sha: &str, workdir: &str) -> PoolEntry {
        PoolEntry::new("ubuntu", "vm", "ssh", workdir, sha, 100.0, 10.0)
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < f64::EPSILON,
            "{actual} != {expected}"
        );
    }

    struct FakeDispatcher {
        status: TargetStatus,
        progress_event: Option<ProgressEvent>,
        cancel_before_progress_event: bool,
        scheduler_defer_reason: Option<String>,
        seen_workdirs: RefCell<Vec<Option<String>>>,
        seen_resume: RefCell<Vec<Option<String>>>,
        seen_durable_progress: RefCell<Vec<TargetResult>>,
        seen_progress_actions: RefCell<Vec<ProgressAction>>,
    }

    impl FakeDispatcher {
        fn new(status: TargetStatus) -> Self {
            Self {
                status,
                progress_event: None,
                cancel_before_progress_event: false,
                scheduler_defer_reason: None,
                seen_workdirs: RefCell::new(Vec::new()),
                seen_resume: RefCell::new(Vec::new()),
                seen_durable_progress: RefCell::new(Vec::new()),
                seen_progress_actions: RefCell::new(Vec::new()),
            }
        }

        fn with_progress_event(mut self, event: ProgressEvent) -> Self {
            self.progress_event = Some(event);
            self
        }

        fn with_cancel_before_progress_event(mut self) -> Self {
            self.cancel_before_progress_event = true;
            self
        }

        fn with_scheduler_defer(mut self, reason: &str) -> Self {
            self.scheduler_defer_reason = Some(reason.to_owned());
            self
        }
    }

    impl ShipTargetDispatcher for FakeDispatcher {
        fn validate(&self, mut request: DispatchValidationRequest<'_, '_>) -> TargetResult {
            self.seen_workdirs
                .borrow_mut()
                .push(request.target.workdir());
            self.seen_resume
                .borrow_mut()
                .push(request.resume_from.clone());
            if let Some(event) = self.progress_event.clone() {
                if self.cancel_before_progress_event {
                    cancel_job_from_log_path(&request.log_path);
                }
                if let Some(callback) = request.progress_callback.as_mut() {
                    self.seen_progress_actions
                        .borrow_mut()
                        .push(callback(event));
                }
                self.seen_durable_progress
                    .borrow_mut()
                    .push(read_target_result_from_queue(
                        &request.log_path,
                        &request.target.name,
                    ));
            }
            let now = Utc::now();
            let mut result = TargetResult::new(
                request.target.name.clone(),
                request.target.platform.clone(),
                self.status,
                request.target.backend_name.clone(),
            );
            result.started_at = Some(now);
            result.completed_at = Some(now);
            result.log_path = Some(request.log_path.to_string_lossy().into_owned());
            result.scheduler_defer_reason = self.scheduler_defer_reason.clone();
            result
        }
    }

    struct SyncDispatcher {
        status: TargetStatus,
        seen_workdirs: Mutex<Vec<Option<String>>>,
    }

    impl SyncDispatcher {
        fn new(status: TargetStatus) -> Self {
            Self {
                status,
                seen_workdirs: Mutex::new(Vec::new()),
            }
        }

        fn seen_count(&self) -> usize {
            self.seen_workdirs.lock().expect("seen lock").len()
        }

        fn workdirs(&self) -> Vec<Option<String>> {
            self.seen_workdirs.lock().expect("seen lock").clone()
        }
    }

    impl ShipTargetDispatcher for SyncDispatcher {
        fn validate(&self, request: DispatchValidationRequest<'_, '_>) -> TargetResult {
            self.seen_workdirs
                .lock()
                .expect("seen lock")
                .push(request.target.workdir());
            let now = Utc::now();
            let mut result = TargetResult::new(
                request.target.name.clone(),
                request.target.platform.clone(),
                self.status,
                request.target.backend_name.clone(),
            );
            result.started_at = Some(now);
            result.completed_at = Some(now);
            result.log_path = Some(request.log_path.to_string_lossy().into_owned());
            result
        }
    }

    struct RefillOrderingDispatcher {
        initial_workers_ready: Barrier,
        refill_started: (Mutex<bool>, Condvar),
        long_observed_refill: Mutex<Option<bool>>,
    }

    impl RefillOrderingDispatcher {
        fn new() -> Self {
            Self {
                initial_workers_ready: Barrier::new(2),
                refill_started: (Mutex::new(false), Condvar::new()),
                long_observed_refill: Mutex::new(None),
            }
        }

        fn long_observed_refill(&self) -> bool {
            self.long_observed_refill
                .lock()
                .expect("long observation lock")
                .expect("long worker observation")
        }

        fn refill_started(&self) -> bool {
            *self.refill_started.0.lock().expect("refill started lock")
        }
    }

    impl ShipTargetDispatcher for RefillOrderingDispatcher {
        fn validate(&self, request: DispatchValidationRequest<'_, '_>) -> TargetResult {
            if matches!(request.target.name.as_str(), "long" | "short") {
                self.initial_workers_ready.wait();
            }
            if request.target.name == "long" {
                let (started_lock, started_condvar) = &self.refill_started;
                let started = started_lock.lock().expect("refill started lock");
                let (started, _) = started_condvar
                    .wait_timeout_while(started, StdDuration::from_secs(2), |started| !*started)
                    .expect("refill start wait");
                *self
                    .long_observed_refill
                    .lock()
                    .expect("long observation lock") = Some(*started);
            } else if request.target.name == "refill" {
                let (started_lock, started_condvar) = &self.refill_started;
                *started_lock.lock().expect("refill started lock") = true;
                started_condvar.notify_all();
            }

            let now = Utc::now();
            let mut result = TargetResult::new(
                request.target.name.clone(),
                request.target.platform.clone(),
                TargetStatus::Pass,
                request.target.backend_name.clone(),
            );
            result.started_at = Some(now);
            result.completed_at = Some(now);
            result.log_path = Some(request.log_path.to_string_lossy().into_owned());
            result
        }
    }

    struct AdmissionFailureDispatcher {
        corrupt_request_path: PathBuf,
        workers_ready: Barrier,
    }

    impl ShipTargetDispatcher for AdmissionFailureDispatcher {
        fn validate(&self, request: DispatchValidationRequest<'_, '_>) -> TargetResult {
            if request.target.name == "short" {
                std::fs::write(&self.corrupt_request_path, "{").expect("corrupt pending request");
            }
            self.workers_ready.wait();

            let now = Utc::now();
            let mut result = TargetResult::new(
                request.target.name.clone(),
                request.target.platform.clone(),
                TargetStatus::Pass,
                request.target.backend_name.clone(),
            );
            result.started_at = Some(now);
            result.completed_at = Some(now);
            result.log_path = Some(request.log_path.to_string_lossy().into_owned());
            if request.target.name == "deferred" {
                result.scheduler_defer_reason = Some("host_pool_lease_unavailable".to_owned());
            }
            result
        }
    }

    fn read_target_result_from_queue(log_path: &std::path::Path, target: &str) -> TargetResult {
        let job_dir = log_path.parent().expect("target log parent");
        let logs_dir = job_dir.parent().expect("logs parent");
        let state_dir = logs_dir.parent().expect("state dir");
        let job_id = job_dir
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("job id");
        let mut queue = Queue::new(state_dir).expect("queue");
        queue
            .get(job_id)
            .expect("queue get")
            .expect("job")
            .results
            .get(target)
            .expect("target result")
            .clone()
    }

    fn cancel_job_from_log_path(log_path: &std::path::Path) {
        let job_dir = log_path.parent().expect("target log parent");
        let logs_dir = job_dir.parent().expect("logs parent");
        let state_dir = logs_dir.parent().expect("state dir");
        let job_id = job_dir
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("job id");
        let mut queue = Queue::new(state_dir).expect("queue");
        let job = queue.get(job_id).expect("queue get").expect("job");
        let cancelled = job
            .cancel_with_reason(Some("cancelled during progress".to_owned()))
            .expect("cancel");
        queue.update(&cancelled).expect("update cancel");
    }

    #[test]
    fn warm_hit_overrides_workdir_and_defaults_resume_to_configure() {
        let target = ssh_target();
        let (_temp, pool) = pool_with(entry("abc", "/srv/warm"));

        let decision = apply_warm_reuse(&pool, &target, "abc", None, false, 20.0);

        assert!(decision.warm_hit);
        assert_eq!(decision.host_key, "vm");
        assert_eq!(decision.resume_from.as_deref(), Some("configure"));
        assert_eq!(decision.target.workdir().as_deref(), Some("/srv/warm"));
    }

    #[test]
    fn requested_later_resume_wins_over_warm_default() {
        let target = ssh_target();
        let (_temp, pool) = pool_with(entry("abc", "/srv/warm"));

        let decision = apply_warm_reuse(&pool, &target, "abc", Some("test"), false, 20.0);

        assert!(decision.warm_hit);
        assert_eq!(decision.resume_from.as_deref(), Some("test"));
    }

    #[test]
    fn sha_miss_preserves_requested_resume_and_original_workdir() {
        let target = ssh_target();
        let (_temp, pool) = pool_with(entry("old", "/srv/warm"));

        let decision = apply_warm_reuse(&pool, &target, "new", Some("build"), false, 20.0);

        assert!(!decision.warm_hit);
        assert_eq!(decision.resume_from.as_deref(), Some("build"));
        assert_eq!(decision.target.workdir().as_deref(), Some("~/repo"));
    }

    #[test]
    fn pass_upserts_warm_pool_entry() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let result = TargetResult::new("ubuntu", "linux-x64", TargetStatus::Pass, "ssh");

        let update = update_warm_pool_after_run(&pool, &target, "abc", &result, false, false, 20.0)
            .expect("update");

        assert_eq!(update, WarmPoolUpdate::Upserted);
        let entry = pool.get("ubuntu", "vm", 21.0).expect("entry");
        assert_eq!(entry.sha, "abc");
        assert_eq!(entry.workdir, "~/repo");
        assert_close(entry.expires_at, 620.0);
    }

    #[test]
    fn failing_warm_reuse_evicts_entry() {
        let target = ssh_target();
        let (_temp, pool) = pool_with(entry("abc", "/srv/warm"));
        let result = TargetResult::new("ubuntu", "linux-x64", TargetStatus::Fail, "ssh");

        let update = update_warm_pool_after_run(&pool, &target, "abc", &result, true, false, 20.0)
            .expect("update");

        assert_eq!(update, WarmPoolUpdate::Evicted);
        assert!(pool.get("ubuntu", "vm", 21.0).is_none());
    }

    #[test]
    fn disabled_pool_does_not_mutate_on_pass() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let result = TargetResult::new("ubuntu", "linux-x64", TargetStatus::Pass, "ssh");

        let update = update_warm_pool_after_run(&pool, &target, "abc", &result, false, true, 20.0)
            .expect("update");

        assert_eq!(update, WarmPoolUpdate::Noop);
        assert!(pool.all_entries().is_empty());
    }

    #[test]
    fn execute_ship_records_queue_evidence_ship_state_and_warm_pool() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);
        let request = ship_request(vec![target]);

        let outcome = execute_ship(
            &request,
            ShipStores {
                queue: &mut queue,
                evidence: &evidence,
                ship_state: &ship_state,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: temp.path(),
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("execute");

        assert!(outcome.job.passed());
        assert!(!outcome.resumed_existing_state);
        assert_eq!(
            queue
                .get(&outcome.job.id)
                .expect("queue")
                .expect("job")
                .status,
            crate::job::JobStatus::Completed
        );
        let evidence_record = evidence
            .get_target_scoped(
                &crate::evidence::repository_ship_evidence_scope(&request.repo, request.pr),
                "feature/test",
                "ubuntu",
            )
            .expect("evidence");
        assert_eq!(evidence_record.status, "pass");
        assert_eq!(evidence_record.host.as_deref(), Some("vm"));
        let state = ship_state.get(42).expect("state");
        assert_eq!(state.pr_url, "https://github.com/danielraffel/pulp/pull/42");
        assert_eq!(state.pr_title, "Test PR");
        assert_eq!(state.evidence_snapshot["ubuntu"], "pass");
        let run = state.get_run("ubuntu").expect("run");
        assert_eq!(run.status, "completed");
        assert_eq!(run.provider, "ssh");
        assert_eq!(run.run_id, outcome.job.id);
        assert!(run.required);
        let request_envelope = QueueRequestStore::new(temp.path())
            .expect("request store")
            .load(&outcome.job.id)
            .expect("load request")
            .expect("request");
        assert_eq!(request_envelope.job_id, outcome.job.id);
        assert_eq!(request_envelope.cwd, temp.path());
        assert!(matches!(
            request_envelope.request,
            QueuedExecutionRequest::Ship(_)
        ));
        let stored_outcome = QueueOutcomeStore::new(temp.path())
            .expect("outcome store")
            .load(&outcome.job.id)
            .expect("load outcome")
            .expect("outcome");
        assert!(matches!(
            stored_outcome,
            QueuedExecutionOutcome::Ship {
                pr: 42,
                resumed_existing_state: false,
                ..
            }
        ));
        let loaded = load_ship_outcome(&mut queue, temp.path(), &outcome.job.id)
            .expect("load durable outcome");
        assert_eq!(loaded, outcome);
        assert!(
            warm_pool
                .get("ubuntu", "vm", crate::warm_pool::now_epoch_secs())
                .is_some()
        );
    }

    #[test]
    fn execute_run_records_request_and_outcome_snapshots() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);
        let request = RunExecutionRequest {
            branch: "feature/run".to_owned(),
            sha: "abc".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![target],
        };

        let outcome = execute_run(
            &request,
            RunStores {
                queue: &mut queue,
                evidence: &evidence,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: temp.path(),
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("execute");

        assert!(outcome.job.passed());
        let request_envelope = QueueRequestStore::new(temp.path())
            .expect("request store")
            .load(&outcome.job.id)
            .expect("load request")
            .expect("request");
        assert_eq!(request_envelope.job_id, outcome.job.id);
        assert_eq!(request_envelope.cwd, temp.path());
        assert!(matches!(
            request_envelope.request,
            QueuedExecutionRequest::Run(_)
        ));
        let stored_outcome = QueueOutcomeStore::new(temp.path())
            .expect("outcome store")
            .load(&outcome.job.id)
            .expect("load outcome")
            .expect("outcome");
        assert_eq!(
            stored_outcome,
            QueuedExecutionOutcome::run(outcome.job.id.clone())
        );
        let loaded =
            load_run_outcome(&mut queue, temp.path(), &outcome.job.id).expect("load durable run");
        assert_eq!(loaded, outcome);
    }

    #[test]
    fn submit_run_records_pending_job_and_request_without_execution() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let request = RunExecutionRequest {
            branch: "feature/run".to_owned(),
            sha: "abc".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![target],
        };

        let job = submit_run(&request, &mut queue, temp.path(), temp.path()).expect("submit");

        assert_eq!(job.status, crate::job::JobStatus::Pending);
        assert_eq!(
            queue
                .get(&job.id)
                .expect("queue")
                .expect("durable job")
                .status,
            crate::job::JobStatus::Pending
        );
        let request_envelope = QueueRequestStore::new(temp.path())
            .expect("request store")
            .load(&job.id)
            .expect("load request")
            .expect("request");
        assert_eq!(request_envelope.job_id, job.id);
        assert!(matches!(
            request_envelope.request,
            QueuedExecutionRequest::Run(_)
        ));
        assert!(
            QueueOutcomeStore::new(temp.path())
                .expect("outcome store")
                .load(&job.id)
                .expect("load outcome")
                .is_none()
        );
    }

    #[test]
    fn cooperative_run_wait_executes_worker_after_acquiring_drain() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);
        let request = RunExecutionRequest {
            branch: "feature/run".to_owned(),
            sha: "abc".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![target],
        };
        let job = submit_run(&request, &mut queue, temp.path(), temp.path()).expect("submit");

        let outcome = drain_or_wait_run(
            &request,
            job,
            RunStores {
                queue: &mut queue,
                evidence: &evidence,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: temp.path(),
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("drain");

        assert!(outcome.job.passed());
        assert_eq!(dispatcher.seen_count(), 1);
        assert_eq!(
            QueueOutcomeStore::new(temp.path())
                .expect("outcome store")
                .load(&outcome.job.id)
                .expect("load outcome"),
            Some(QueuedExecutionOutcome::run(outcome.job.id))
        );
    }

    #[test]
    fn foreground_recovery_preserves_daemon_owned_running_jobs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let request = RunExecutionRequest {
            branch: "feature/run".to_owned(),
            sha: "abc".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![ssh_target()],
        };
        let daemon_job =
            submit_run(&request, &mut queue, temp.path(), &state_dir).expect("daemon submit");
        let mut foreground_request = request.clone();
        foreground_request.branch = "feature/foreground".to_owned();
        let foreground_job = submit_run(&foreground_request, &mut queue, temp.path(), &state_dir)
            .expect("foreground submit");
        let mut mismatched_request = request.clone();
        mismatched_request.branch = "feature/mismatched".to_owned();
        let mismatched_job = submit_run(&mismatched_request, &mut queue, temp.path(), &state_dir)
            .expect("mismatched submit");
        let store = QueueRequestStore::new(&state_dir).expect("store");
        let mut daemon_envelope = store
            .load(&daemon_job.id)
            .expect("load")
            .expect("daemon envelope");
        daemon_envelope.execution_owner = QueuedExecutionOwner::Daemon;
        store.save(&daemon_envelope).expect("mark daemon owned");
        let mut mismatched_envelope = store
            .load(&mismatched_job.id)
            .expect("load")
            .expect("mismatched envelope");
        mismatched_envelope.job_id = "different-job".to_owned();
        std::fs::write(
            store.path_for(&mismatched_job.id),
            serde_json::to_vec(&mismatched_envelope).expect("serialize"),
        )
        .expect("swap mismatched envelope");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        queue
            .start_pending_jobs_for_drain(
                &lock,
                &[
                    daemon_job.id.clone(),
                    foreground_job.id.clone(),
                    mismatched_job.id.clone(),
                ],
            )
            .expect("start jobs");

        let recovered = recover_foreground_running_jobs_for_drain(&mut queue, &lock, &state_dir)
            .expect("recover foreground ownership");

        assert_eq!(
            recovered
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            [foreground_job.id.as_str()]
        );
        assert_eq!(
            queue
                .get(&daemon_job.id)
                .expect("read")
                .expect("daemon job")
                .status,
            JobStatus::Running
        );
        assert_eq!(
            queue
                .get(&foreground_job.id)
                .expect("read")
                .expect("foreground job")
                .status,
            JobStatus::Completed
        );
        assert_eq!(
            queue
                .get(&mismatched_job.id)
                .expect("read")
                .expect("mismatched job")
                .status,
            JobStatus::Running
        );
    }

    #[test]
    fn foreground_drain_never_admits_daemon_owned_pending_job() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let daemon_request = RunExecutionRequest {
            branch: "feature/daemon".to_owned(),
            sha: "daemon".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::High,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![ssh_target()],
        };
        let mut foreground_request = daemon_request.clone();
        foreground_request.branch = "feature/foreground".to_owned();
        foreground_request.sha = "foreground".to_owned();
        foreground_request.priority = Priority::Normal;
        let daemon_job = submit_run(&daemon_request, &mut queue, temp.path(), &state_dir)
            .expect("daemon submit");
        let foreground_job = submit_run(&foreground_request, &mut queue, temp.path(), &state_dir)
            .expect("foreground submit");
        let store = QueueRequestStore::new(&state_dir).expect("store");
        let mut daemon_envelope = store
            .load(&daemon_job.id)
            .expect("load")
            .expect("daemon envelope");
        daemon_envelope.execution_owner = QueuedExecutionOwner::Daemon;
        store.save(&daemon_envelope).expect("mark daemon owned");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);

        let outcome = drain_or_wait_run(
            &foreground_request,
            foreground_job,
            RunStores {
                queue: &mut queue,
                evidence: &evidence,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: &state_dir,
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("foreground drain");

        assert!(outcome.job.passed());
        assert_eq!(dispatcher.seen_count(), 1);
        assert_eq!(
            queue
                .get(&daemon_job.id)
                .expect("read")
                .expect("daemon job")
                .status,
            JobStatus::Pending
        );
    }

    #[test]
    fn cooperative_drain_uses_each_queued_jobs_submitted_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let first_cwd = temp.path().join("first");
        let second_cwd = temp.path().join("second");
        std::fs::create_dir_all(&first_cwd).expect("first cwd");
        std::fs::create_dir_all(&second_cwd).expect("second cwd");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);
        let first_request = RunExecutionRequest {
            branch: "feature/first".to_owned(),
            sha: "first".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![local_target_without_cwd("first", "linux-x64")],
        };
        let second_request = RunExecutionRequest {
            branch: "feature/second".to_owned(),
            sha: "second".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![local_target_without_cwd("second", "linux-x64")],
        };
        let first_job =
            submit_run(&first_request, &mut queue, &first_cwd, &state_dir).expect("submit first");
        let second_job = submit_run(&second_request, &mut queue, &second_cwd, &state_dir)
            .expect("submit second");

        let outcome = drain_or_wait_run(
            &first_request,
            first_job,
            RunStores {
                queue: &mut queue,
                evidence: &evidence,
                warm_pool: &warm_pool,
                cwd: &first_cwd,
                state_dir: &state_dir,
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("drain");

        assert!(outcome.job.passed());
        assert_eq!(
            queue
                .get(&second_job.id)
                .expect("queue")
                .expect("second job")
                .status,
            JobStatus::Completed
        );
        let mut workdirs = dispatcher
            .workdirs()
            .into_iter()
            .map(|path| path.expect("local cwd"))
            .collect::<Vec<_>>();
        workdirs.sort();
        let mut expected = vec![
            first_cwd.to_string_lossy().into_owned(),
            second_cwd.to_string_lossy().into_owned(),
        ];
        expected.sort();
        assert_eq!(workdirs, expected);
    }

    #[test]
    fn drain_refills_a_worker_slot_before_a_slower_sibling_finishes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship state");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let config = empty_config(temp.path());
        let dispatcher = RefillOrderingDispatcher::new();
        let request = |name: &str, priority| RunExecutionRequest {
            branch: format!("feature/{name}"),
            sha: name.to_owned(),
            mode: ValidationMode::Full,
            priority,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![local_target_without_cwd(name, "linux-x64")],
        };
        let long = request("long", Priority::High);
        let short = request("short", Priority::Normal);
        let refill = request("refill", Priority::Low);
        let jobs = [(&long, "long"), (&short, "short"), (&refill, "refill")]
            .into_iter()
            .map(|(request, cwd)| {
                let cwd = temp.path().join(cwd);
                std::fs::create_dir_all(&cwd).expect("job cwd");
                submit_run(request, &mut queue, &cwd, &state_dir).expect("submit")
            })
            .collect::<Vec<_>>();
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");

        run_drain_worker_cycle(
            &mut queue,
            &drain_lock,
            &evidence,
            &ship_state,
            &warm_pool,
            temp.path(),
            &state_dir,
            &config,
            &jobs[0].id,
            &dispatcher,
            None,
        )
        .expect("drain cycle");

        assert!(
            dispatcher.long_observed_refill(),
            "the third worker should start while the first worker is still blocked"
        );
        for job in jobs {
            assert_eq!(
                queue.get(&job.id).expect("queue").expect("job").status,
                JobStatus::Completed
            );
        }
    }

    #[test]
    fn drain_stops_refilling_after_the_awaited_job_finishes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship state");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let config = empty_config(temp.path());
        let dispatcher = RefillOrderingDispatcher::new();
        let request = |name: &str, priority| RunExecutionRequest {
            branch: format!("feature/{name}"),
            sha: name.to_owned(),
            mode: ValidationMode::Full,
            priority,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![local_target_without_cwd(name, "linux-x64")],
        };
        let long_cwd = temp.path().join("long");
        let short_cwd = temp.path().join("short");
        let refill_cwd = temp.path().join("refill");
        for cwd in [&long_cwd, &short_cwd, &refill_cwd] {
            std::fs::create_dir_all(cwd).expect("job cwd");
        }
        let long = submit_run(
            &request("long", Priority::High),
            &mut queue,
            &long_cwd,
            &state_dir,
        )
        .expect("submit long");
        let short = submit_run(
            &request("short", Priority::Normal),
            &mut queue,
            &short_cwd,
            &state_dir,
        )
        .expect("submit short");
        let refill = submit_run(
            &request("refill", Priority::Low),
            &mut queue,
            &refill_cwd,
            &state_dir,
        )
        .expect("submit refill");
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");

        run_drain_worker_cycle(
            &mut queue,
            &drain_lock,
            &evidence,
            &ship_state,
            &warm_pool,
            temp.path(),
            &state_dir,
            &config,
            &short.id,
            &dispatcher,
            None,
        )
        .expect("drain cycle");

        assert!(!dispatcher.refill_started());
        assert!(!dispatcher.long_observed_refill());
        assert_eq!(
            queue.get(&long.id).expect("queue").expect("long").status,
            JobStatus::Completed
        );
        assert_eq!(
            queue.get(&short.id).expect("queue").expect("short").status,
            JobStatus::Completed
        );
        assert_eq!(
            queue
                .get(&refill.id)
                .expect("queue")
                .expect("refill")
                .status,
            JobStatus::Pending
        );
    }

    #[test]
    fn drain_does_not_admit_when_the_awaited_job_is_already_terminal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship state");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let config = empty_config(temp.path());
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);
        let request = |name: &str, priority| RunExecutionRequest {
            branch: format!("feature/{name}"),
            sha: name.to_owned(),
            mode: ValidationMode::Full,
            priority,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![local_target_without_cwd(name, "linux-x64")],
        };
        let awaited_cwd = temp.path().join("awaited");
        let unrelated_cwd = temp.path().join("unrelated");
        std::fs::create_dir_all(&awaited_cwd).expect("awaited cwd");
        std::fs::create_dir_all(&unrelated_cwd).expect("unrelated cwd");
        let awaited = submit_run(
            &request("awaited", Priority::High),
            &mut queue,
            &awaited_cwd,
            &state_dir,
        )
        .expect("submit awaited");
        let unrelated = submit_run(
            &request("unrelated", Priority::Normal),
            &mut queue,
            &unrelated_cwd,
            &state_dir,
        )
        .expect("submit unrelated");
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");
        let completed = queue
            .start_pending_jobs_for_drain(&drain_lock, std::slice::from_ref(&awaited.id))
            .expect("start awaited")
            .pop()
            .expect("started awaited")
            .complete()
            .expect("complete awaited");
        queue.update(&completed).expect("persist completed awaited");

        run_drain_worker_cycle(
            &mut queue,
            &drain_lock,
            &evidence,
            &ship_state,
            &warm_pool,
            temp.path(),
            &state_dir,
            &config,
            &awaited.id,
            &dispatcher,
            None,
        )
        .expect("drain cycle");

        assert_eq!(dispatcher.seen_count(), 0);
        assert_eq!(
            queue
                .get(&unrelated.id)
                .expect("queue")
                .expect("unrelated")
                .status,
            JobStatus::Pending
        );
    }

    #[test]
    fn drain_does_not_start_replacements_when_admission_cancels_the_awaited_job() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship state");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let config = empty_config(temp.path());
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);
        let target = local_target_without_cwd("local", "linux-x64");
        let old_cwd = temp.path().join("old");
        let new_cwd = temp.path().join("new");
        std::fs::create_dir_all(&old_cwd).expect("old cwd");
        std::fs::create_dir_all(&new_cwd).expect("new cwd");
        let mut old_request = ship_request(vec![target.clone()]);
        old_request.branch = "feature/old".to_owned();
        old_request.sha = "old".to_owned();
        let mut new_request = ship_request(vec![target]);
        new_request.branch = "feature/new".to_owned();
        new_request.sha = "new".to_owned();
        let old =
            submit_ship(&old_request, &mut queue, &old_cwd, &state_dir).expect("submit old ship");
        let new =
            submit_ship(&new_request, &mut queue, &new_cwd, &state_dir).expect("submit new ship");
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");

        run_drain_worker_cycle(
            &mut queue,
            &drain_lock,
            &evidence,
            &ship_state,
            &warm_pool,
            temp.path(),
            &state_dir,
            &config,
            &old.id,
            &dispatcher,
            None,
        )
        .expect("drain cycle");

        assert_eq!(dispatcher.seen_count(), 0);
        assert_eq!(
            queue.get(&old.id).expect("queue").expect("old").status,
            JobStatus::Cancelled
        );
        assert_eq!(
            queue.get(&new.id).expect("queue").expect("new").status,
            JobStatus::Pending
        );
    }

    #[test]
    fn drain_preserves_refill_error_after_requeueing_active_deferred_worker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship state");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let config = empty_config(temp.path());
        let request = |name: &str, priority| RunExecutionRequest {
            branch: format!("feature/{name}"),
            sha: name.to_owned(),
            mode: ValidationMode::Full,
            priority,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![local_target_without_cwd(name, "linux-x64")],
        };
        let deferred_cwd = temp.path().join("deferred");
        let short_cwd = temp.path().join("short");
        let pending_cwd = temp.path().join("pending");
        for cwd in [&deferred_cwd, &short_cwd, &pending_cwd] {
            std::fs::create_dir_all(cwd).expect("job cwd");
        }
        let deferred = submit_run(
            &request("deferred", Priority::High),
            &mut queue,
            &deferred_cwd,
            &state_dir,
        )
        .expect("submit deferred");
        let short = submit_run(
            &request("short", Priority::Normal),
            &mut queue,
            &short_cwd,
            &state_dir,
        )
        .expect("submit short");
        let pending = submit_run(
            &request("pending", Priority::Low),
            &mut queue,
            &pending_cwd,
            &state_dir,
        )
        .expect("submit pending");
        let request_store = QueueRequestStore::new(&state_dir).expect("request store");
        let dispatcher = AdmissionFailureDispatcher {
            corrupt_request_path: request_store.path_for(&pending.id),
            workers_ready: Barrier::new(2),
        };
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");

        let error = run_drain_worker_cycle(
            &mut queue,
            &drain_lock,
            &evidence,
            &ship_state,
            &warm_pool,
            temp.path(),
            &state_dir,
            &config,
            &pending.id,
            &dispatcher,
            None,
        )
        .expect_err("corrupt pending request must fail refill admission");

        assert!(matches!(error, ShipExecutionError::QueueRequest(_)));
        assert_eq!(
            queue.get(&short.id).expect("queue").expect("short").status,
            JobStatus::Completed
        );
        let deferred = queue.get(&deferred.id).expect("queue").expect("deferred");
        assert_eq!(deferred.status, JobStatus::Pending);
        assert_eq!(deferred.scheduler_defer_count, 1);
        assert_eq!(
            deferred.scheduler_defer_reason.as_deref(),
            Some("host_pool_lease_unavailable")
        );
        assert!(deferred.scheduler_defer_until.is_some());
        assert_eq!(
            queue
                .get(&pending.id)
                .expect("queue")
                .expect("pending")
                .status,
            JobStatus::Pending
        );
    }

    #[test]
    fn cooperative_run_wait_does_not_dispatch_without_drain_ownership() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);
        let request = RunExecutionRequest {
            branch: "feature/run".to_owned(),
            sha: "abc".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![target],
        };
        let job = submit_run(&request, &mut queue, temp.path(), &state_dir).expect("submit");
        let _held_drain = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");

        let error = drain_or_wait_run_with_options(
            &request,
            job,
            RunStores {
                queue: &mut queue,
                evidence: &evidence,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: &state_dir,
                config: &empty_config(temp.path()),
            },
            &dispatcher,
            CooperativeDrainOptions {
                poll_interval: StdDuration::ZERO,
                max_wait_iterations: Some(0),
            },
        )
        .expect_err("wait timeout");

        assert!(matches!(
            error,
            ShipExecutionError::CooperativeWaitTimedOut(_)
        ));
        assert_eq!(dispatcher.seen_count(), 0);
    }

    #[test]
    fn drain_admission_defers_macos_when_vm_slots_exhausted_but_runs_linux() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mac_cwd = temp.path().join("mac");
        let linux_cwd = temp.path().join("linux");
        std::fs::create_dir_all(&mac_cwd).expect("mac cwd");
        std::fs::create_dir_all(&linux_cwd).expect("linux cwd");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = SyncDispatcher::new(TargetStatus::Pass);
        let config = macos_zero_capacity_config(temp.path());
        let mac_request = RunExecutionRequest {
            branch: "feature/mac".to_owned(),
            sha: "mac".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![local_target("mac", "macos-arm64", &mac_cwd)],
        };
        let linux_request = RunExecutionRequest {
            branch: "feature/linux".to_owned(),
            sha: "linux".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![local_target("linux", "linux-x64", &linux_cwd)],
        };
        let mac_job =
            submit_run(&mac_request, &mut queue, temp.path(), &state_dir).expect("submit mac");
        let linux_job =
            submit_run(&linux_request, &mut queue, temp.path(), &state_dir).expect("submit linux");

        let error = drain_or_wait_run_with_options(
            &mac_request,
            mac_job.clone(),
            RunStores {
                queue: &mut queue,
                evidence: &evidence,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: &state_dir,
                config: &config,
            },
            &dispatcher,
            CooperativeDrainOptions {
                poll_interval: StdDuration::ZERO,
                max_wait_iterations: Some(0),
            },
        )
        .expect_err("mac job remains queued");

        assert!(matches!(
            error,
            ShipExecutionError::CooperativeWaitTimedOut(job_id) if job_id == mac_job.id
        ));
        assert_eq!(
            queue
                .get(&mac_job.id)
                .expect("queue")
                .expect("mac job")
                .status,
            JobStatus::Pending
        );
        assert_eq!(
            queue
                .get(&linux_job.id)
                .expect("queue")
                .expect("linux job")
                .status,
            JobStatus::Completed
        );
        assert_eq!(dispatcher.seen_count(), 1);
    }

    #[test]
    fn refill_lease_snapshot_excludes_only_leases_covered_by_running_reservations() {
        let now = Utc::now();
        let lease = |lease_id: &str, pool_name: &str, job_id: Option<&str>| HostPoolLease {
            lease_id: lease_id.to_owned(),
            pool_name: pool_name.to_owned(),
            member_id: "mac-a".to_owned(),
            target_name: "mac".to_owned(),
            backend: "local".to_owned(),
            host: None,
            job_id: job_id.map(ToOwned::to_owned),
            branch: "feature".to_owned(),
            sha: "abc".to_owned(),
            owner_pid: 1,
            acquired_at: now,
            heartbeat_at: now,
            expires_at: now + Duration::minutes(1),
        };
        let mut reservations = [(
            "running".to_owned(),
            "macs".to_owned(),
            BTreeSet::from(["mac-a".to_owned()]),
            1,
        )];

        let visible = leases_not_covered_by_running_reservations(
            &mut reservations,
            vec![
                lease("covered", "macs", Some("running")),
                HostPoolLease {
                    member_id: "mac-b".to_owned(),
                    ..lease("fallback-capability", "macs", Some("running"))
                },
                lease("fallback-pool", "fallback-macs", Some("running")),
                lease("external", "macs", None),
            ],
        );

        assert_eq!(
            visible
                .iter()
                .map(|lease| lease.lease_id.as_str())
                .collect::<Vec<_>>(),
            ["fallback-capability", "fallback-pool", "external"]
        );
    }

    #[test]
    fn run_worker_honors_durable_cancel_before_start() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = FakeDispatcher::new(TargetStatus::Pass);
        let request = RunExecutionRequest {
            branch: "feature/run".to_owned(),
            sha: "abc".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![target],
        };
        let job = submit_run(&request, &mut queue, temp.path(), temp.path()).expect("submit");
        let cancelled = job
            .cancel_with_reason(Some("user requested cancellation".to_owned()))
            .expect("cancel");
        queue.update(&cancelled).expect("update cancel");

        let outcome = execute_run_worker(
            &request,
            job,
            RunStores {
                queue: &mut queue,
                evidence: &evidence,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: temp.path(),
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("worker");

        assert_eq!(outcome.job.status, JobStatus::Cancelled);
        assert!(dispatcher.seen_workdirs.borrow().is_empty());
        assert!(
            QueueOutcomeStore::new(temp.path())
                .expect("outcome store")
                .load(&outcome.job.id)
                .expect("load outcome")
                .is_none()
        );
    }

    #[test]
    fn run_worker_honors_durable_cancel_from_progress_callback() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = FakeDispatcher::new(TargetStatus::Pass)
            .with_progress_event(ProgressEvent::phase("build"))
            .with_cancel_before_progress_event();
        let request = RunExecutionRequest {
            branch: "feature/run".to_owned(),
            sha: "abc".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![target],
        };
        let job = submit_run(&request, &mut queue, temp.path(), &state_dir).expect("submit");

        let outcome = execute_run_worker(
            &request,
            job,
            RunStores {
                queue: &mut queue,
                evidence: &evidence,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: &state_dir,
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("worker");

        assert_eq!(outcome.job.status, JobStatus::Cancelled);
        assert_eq!(
            outcome.job.cancellation_reason.as_deref(),
            Some("cancelled during progress")
        );
        assert_eq!(dispatcher.seen_workdirs.borrow().len(), 1);
        assert_eq!(
            dispatcher.seen_progress_actions.borrow().as_slice(),
            &[ProgressAction::Terminate(
                "cancelled during progress".to_owned()
            )]
        );
        assert!(
            QueueOutcomeStore::new(&state_dir)
                .expect("outcome store")
                .load(&outcome.job.id)
                .expect("load outcome")
                .is_some()
        );
    }

    #[test]
    fn run_worker_accepts_job_started_by_drain_owner() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = FakeDispatcher::new(TargetStatus::Pass);
        let request = RunExecutionRequest {
            branch: "feature/run".to_owned(),
            sha: "abc".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![target],
        };
        let job = submit_run(&request, &mut queue, temp.path(), &state_dir).expect("submit");
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("held");
        let started = queue
            .start_pending_jobs_for_drain(&drain_lock, std::slice::from_ref(&job.id))
            .expect("start")
            .pop()
            .expect("started");
        let started_at = started.started_at;

        let outcome = execute_run_worker(
            &request,
            started,
            RunStores {
                queue: &mut queue,
                evidence: &evidence,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: &state_dir,
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("worker");

        assert_eq!(outcome.job.status, JobStatus::Completed);
        assert_eq!(outcome.job.started_at, started_at);
        assert_eq!(dispatcher.seen_workdirs.borrow().len(), 1);
    }

    #[test]
    fn scheduler_deferred_target_is_not_persisted_as_final_result() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = FakeDispatcher::new(TargetStatus::Pending)
            .with_scheduler_defer("host_pool_lease_unavailable");
        let request = ship_request(vec![target]);
        let job = submit_ship(&request, &mut queue, temp.path(), &state_dir).expect("submit");
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("held");
        let started = queue
            .start_pending_jobs_for_drain(&drain_lock, std::slice::from_ref(&job.id))
            .expect("start")
            .pop()
            .expect("started");

        let outcome = execute_targets_with_options(
            &request,
            &state_dir,
            &mut queue,
            &warm_pool,
            &dispatcher,
            started.clone(),
            super::TargetExecOptions {
                cwd: temp.path(),
                defer_host_pool_lease_unavailable: true,
                reclassify_vitals_path: None,
                transient_retry: crate::ship_retry::TransientRetryPolicy::disabled(),
                config: None,
            },
        )
        .expect("targets");

        match outcome {
            super::TargetExecutionOutcome::Deferred { job, reason } => {
                assert_eq!(job.id, started.id);
                assert_eq!(reason, "host_pool_lease_unavailable");
            }
            super::TargetExecutionOutcome::Completed(job) => {
                panic!("expected scheduler deferral, got {job:?}");
            }
        }
        let durable = queue.get(&started.id).expect("queue").expect("durable job");
        let result = durable.results.get("ubuntu").expect("running result");
        assert_eq!(result.status, TargetStatus::Running);
        assert_eq!(result.scheduler_defer_reason, None);
    }

    #[test]
    fn drain_admit_worker_cap_respects_already_running_jobs() {
        let mut running = crate::job::Job::create(
            "sha-running",
            "feature/running",
            vec!["mac".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        )
        .start()
        .expect("start");
        running.id = "running".to_owned();
        let mut pass = RequestBackedAdmitPass {
            plan: AdmitPassPlan {
                admitted: vec!["job-a".to_owned(), "job-b".to_owned(), "job-c".to_owned()],
                ..AdmitPassPlan::default()
            },
            running_request_errors: Vec::new(),
            same_pr_ship_admission: SamePrShipAdmission::default(),
            already_merged_cancellations: Vec::new(),
        };

        cap_admit_pass_workers(&[running], &mut pass, 2);

        assert_eq!(pass.plan.admitted, ["job-a"]);
    }

    #[test]
    fn submit_ship_records_pending_job_and_request_without_ship_state() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let request = ship_request(vec![target]);

        let job = submit_ship(&request, &mut queue, temp.path(), temp.path()).expect("submit");

        assert_eq!(job.status, crate::job::JobStatus::Pending);
        assert_eq!(
            queue
                .get(&job.id)
                .expect("queue")
                .expect("durable job")
                .status,
            crate::job::JobStatus::Pending
        );
        assert!(ship_state.get(request.pr).is_none());
        let request_envelope = QueueRequestStore::new(temp.path())
            .expect("request store")
            .load(&job.id)
            .expect("load request")
            .expect("request");
        assert_eq!(request_envelope.job_id, job.id);
        assert!(matches!(
            request_envelope.request,
            QueuedExecutionRequest::Ship(_)
        ));
        assert!(
            QueueOutcomeStore::new(temp.path())
                .expect("outcome store")
                .load(&job.id)
                .expect("load outcome")
                .is_none()
        );
    }

    #[test]
    fn submit_ship_refuses_when_same_pr_ship_is_running() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let request = ship_request(vec![target.clone()]);
        let running_job =
            submit_ship(&request, &mut queue, temp.path(), &state_dir).expect("submit existing");
        let drain = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");
        let started = queue
            .start_pending_jobs_for_drain(&drain, std::slice::from_ref(&running_job.id))
            .expect("start");
        assert_eq!(started.len(), 1);

        let mut case_alias = ship_request(vec![target]);
        case_alias.repo = request.repo.to_ascii_uppercase();
        let error = submit_ship(&case_alias, &mut queue, temp.path(), &state_dir)
            .expect_err("case-only repository alias is the same running PR");

        assert!(matches!(
            error,
            ShipExecutionError::SamePrShipRunning {
                pr: 42,
                running_job_id,
                ..
            } if running_job_id == running_job.id
        ));
    }

    #[test]
    fn daemon_ship_durable_envelope_failure_prevents_queue_admission() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).expect("repo dir");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "shipyard@example.invalid"],
            vec!["config", "user.name", "Shipyard Test"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/danielraffel/pulp.git",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .status()
                    .expect("git setup")
                    .success()
            );
        }
        std::fs::write(repo.join("tracked"), "fixture\n").expect("tracked file");
        assert!(
            std::process::Command::new("git")
                .args(["add", "tracked"])
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
            .expect("git head");
        assert!(head.status.success());

        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let mut request = ship_request(vec![ssh_target()]);
        request.sha = String::from_utf8(head.stdout)
            .expect("utf8 head")
            .trim()
            .to_owned();
        let config = empty_config(&repo);
        let persistence_attempted = std::cell::Cell::new(false);
        let error = super::submit_ship_with_config_and_persist(
            &request,
            &mut queue,
            &repo,
            &state_dir,
            Some(&config),
            |_, envelope, durable| {
                assert!(durable, "daemon-owned envelopes require fsync durability");
                assert_eq!(envelope.execution_owner, QueuedExecutionOwner::Daemon);
                persistence_attempted.set(true);
                Err(QueueRequestError::Io(std::io::Error::other(
                    "injected durable persistence failure",
                )))
            },
        )
        .expect_err("durable envelope failure must abort submission");

        assert!(persistence_attempted.get());
        assert!(matches!(
            error,
            ShipExecutionError::QueueRequest(QueueRequestError::Io(_))
        ));
        assert!(
            queue.get_all().expect("queue jobs").is_empty(),
            "queue admission must happen only after the durable envelope commits"
        );
    }

    #[test]
    fn submit_ship_reaps_stale_same_pr_running_ship_and_enqueues_retry() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let request = ship_request(vec![target.clone()]);
        let running_job =
            submit_ship(&request, &mut queue, temp.path(), &state_dir).expect("submit existing");
        let drain = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");
        let started = queue
            .start_pending_jobs_for_drain(&drain, std::slice::from_ref(&running_job.id))
            .expect("start");
        assert_eq!(started.len(), 1);

        // Age the started worker past the staleness threshold to simulate a
        // killed worker that stopped heartbeating.
        let mut aged = queue.get(&running_job.id).expect("get").expect("running");
        aged.started_at = Some(
            Utc::now() - Duration::seconds(crate::job::DEFAULT_RUNNING_JOB_STALE_SECONDS + 60),
        );
        queue.update(&aged).expect("age running job");

        // The retry now succeeds: the stale running job is reaped rather than
        // blocking the same PR forever.
        let retry = submit_ship(
            &ship_request(vec![target]),
            &mut queue,
            temp.path(),
            &state_dir,
        )
        .expect("retry submits after reaping stale same-PR ship");
        assert_ne!(retry.id, running_job.id);
        assert_eq!(retry.status, crate::job::JobStatus::Pending);

        let reaped = queue.get(&running_job.id).expect("get").expect("reaped");
        assert_eq!(reaped.status, crate::job::JobStatus::Cancelled);
        assert_eq!(
            reaped.cancellation_reason.as_deref(),
            Some(crate::queue::STALE_RUNNING_CANCEL_REASON)
        );
    }

    #[test]
    fn submit_ship_never_reaps_stale_daemon_owned_same_pr_running() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let request = ship_request(vec![target.clone()]);
        let running_job =
            submit_ship(&request, &mut queue, temp.path(), &state_dir).expect("submit existing");
        let store = QueueRequestStore::new(&state_dir).expect("store");
        let mut envelope = store.load(&running_job.id).expect("load").expect("request");
        envelope.execution_owner = QueuedExecutionOwner::Daemon;
        store.save(&envelope).expect("save daemon owner");
        let drain = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");
        queue
            .start_pending_jobs_for_drain(&drain, std::slice::from_ref(&running_job.id))
            .expect("start");
        let mut aged = queue.get(&running_job.id).expect("get").expect("running");
        aged.started_at = Some(
            Utc::now() - Duration::seconds(crate::job::DEFAULT_RUNNING_JOB_STALE_SECONDS + 60),
        );
        queue.update(&aged).expect("age running job");

        let error = submit_ship(
            &ship_request(vec![target]),
            &mut queue,
            temp.path(),
            &state_dir,
        )
        .expect_err("daemon-owned running job remains fenced");

        assert!(matches!(
            error,
            ShipExecutionError::SamePrShipRunning { .. }
        ));
        assert_eq!(
            queue
                .get(&running_job.id)
                .expect("get")
                .expect("running")
                .status,
            JobStatus::Running
        );
    }

    #[test]
    fn submit_ship_fails_closed_for_mismatched_running_envelope() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let request = ship_request(vec![target.clone()]);
        let running =
            submit_ship(&request, &mut queue, temp.path(), &state_dir).expect("submit existing");
        let store = QueueRequestStore::new(&state_dir).expect("store");
        let mut envelope = store.load(&running.id).expect("load").expect("request");
        envelope.job_id = "different-job".to_owned();
        std::fs::write(
            store.path_for(&running.id),
            serde_json::to_vec(&envelope).expect("encode"),
        )
        .expect("write mismatch");
        let drain = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");
        queue
            .start_pending_jobs_for_drain(&drain, std::slice::from_ref(&running.id))
            .expect("start");

        let error = submit_ship(
            &ship_request(vec![target]),
            &mut queue,
            temp.path(),
            &state_dir,
        )
        .expect_err("mismatched running ownership is unknown");

        assert!(matches!(
            error,
            ShipExecutionError::QueueRequest(QueueRequestError::InvalidSnapshot { .. })
        ));
        assert_eq!(
            queue.get(&running.id).expect("get").expect("job").status,
            JobStatus::Running
        );
    }

    #[test]
    fn daemon_worker_refuses_mismatched_embedded_job_id_before_execution() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let request = RunExecutionRequest {
            branch: "feature/run".to_owned(),
            sha: "abc".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: vec![ssh_target()],
        };
        let job = submit_run(&request, &mut queue, temp.path(), &state_dir).expect("submit");
        let store = QueueRequestStore::new(&state_dir).expect("store");
        let mut envelope = store.load(&job.id).expect("load").expect("request");
        envelope.job_id = "different-job".to_owned();
        std::fs::write(
            store.path_for(&job.id),
            serde_json::to_vec(&envelope).expect("encode"),
        )
        .expect("write mismatch");

        let error = super::execute_started_queued_job(
            &job.id,
            RuntimeMode::Isolated,
            temp.path(),
            &state_dir,
        )
        .expect_err("mismatched worker envelope");

        assert!(matches!(
            error,
            ShipExecutionError::QueueRequest(QueueRequestError::InvalidSnapshot { .. })
        ));
    }

    #[test]
    fn ship_worker_honors_durable_cancel_before_start_without_ship_state() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = FakeDispatcher::new(TargetStatus::Pass);
        let request = ship_request(vec![target]);
        let job = submit_ship(&request, &mut queue, temp.path(), temp.path()).expect("submit");
        let cancelled = job
            .cancel_with_reason(Some("user requested cancellation".to_owned()))
            .expect("cancel");
        queue.update(&cancelled).expect("update cancel");

        let outcome = execute_ship_worker(
            &request,
            job,
            ShipStores {
                queue: &mut queue,
                evidence: &evidence,
                ship_state: &ship_state,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: temp.path(),
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("worker");

        assert_eq!(outcome.job.status, JobStatus::Cancelled);
        assert!(dispatcher.seen_workdirs.borrow().is_empty());
        assert!(ship_state.get(request.pr).is_none());
        assert!(
            QueueOutcomeStore::new(temp.path())
                .expect("outcome store")
                .load(&outcome.job.id)
                .expect("load outcome")
                .is_none()
        );
    }

    #[test]
    fn ship_worker_accepts_job_started_by_drain_owner() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = FakeDispatcher::new(TargetStatus::Pass);
        let request = ship_request(vec![target]);
        let job = submit_ship(&request, &mut queue, temp.path(), &state_dir).expect("submit");
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("held");
        let started = queue
            .start_pending_jobs_for_drain(&drain_lock, std::slice::from_ref(&job.id))
            .expect("start")
            .pop()
            .expect("started");
        let started_at = started.started_at;

        let outcome = execute_ship_worker(
            &request,
            started,
            ShipStores {
                queue: &mut queue,
                evidence: &evidence,
                ship_state: &ship_state,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: &state_dir,
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("worker");

        assert_eq!(outcome.job.status, JobStatus::Completed);
        assert_eq!(outcome.job.started_at, started_at);
        assert!(ship_state.get(request.pr).is_some());
        assert_eq!(dispatcher.seen_workdirs.borrow().len(), 1);
    }

    #[test]
    fn execute_ship_marks_advisory_targets_non_required() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = FakeDispatcher::new(TargetStatus::Fail);
        let mut request = ship_request(vec![target]);
        request.advisory_targets.insert("ubuntu".to_owned());

        let outcome = execute_ship(
            &request,
            ShipStores {
                queue: &mut queue,
                evidence: &evidence,
                ship_state: &ship_state,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: temp.path(),
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("execute");

        assert!(!outcome.job.passed());
        let state = ship_state.get(42).expect("state");
        let run = state.get_run("ubuntu").expect("run");
        assert!(!run.required);
        assert_eq!(state.evidence_snapshot["ubuntu"], "fail");
    }

    #[test]
    fn execute_ship_persists_streaming_progress_before_target_finishes() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let heartbeat = Utc::now() - Duration::seconds(11);
        let dispatcher =
            FakeDispatcher::new(TargetStatus::Pass).with_progress_event(ProgressEvent {
                phase: Some("build".to_owned()),
                last_output_at: Some(heartbeat),
                last_heartbeat_at: heartbeat,
                quiet_for_secs: 11.0,
                liveness: "quiet".to_owned(),
            });
        let request = ship_request(vec![target]);

        let outcome = execute_ship(
            &request,
            ShipStores {
                queue: &mut queue,
                evidence: &evidence,
                ship_state: &ship_state,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: &state_dir,
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("execute");

        assert!(outcome.job.passed());
        let durable_progress = dispatcher.seen_durable_progress.borrow();
        assert_eq!(durable_progress.len(), 1);
        assert_eq!(durable_progress[0].status, TargetStatus::Running);
        assert_eq!(durable_progress[0].phase.as_deref(), Some("build"));
        assert_eq!(durable_progress[0].last_output_at, Some(heartbeat));
        assert_eq!(durable_progress[0].last_heartbeat_at, Some(heartbeat));
        assert_eq!(durable_progress[0].quiet_for_secs, Some(11.0));
        assert_eq!(durable_progress[0].liveness.as_deref(), Some("quiet"));
    }

    #[test]
    fn execute_ship_applies_and_evicts_failed_warm_reuse() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let now = crate::warm_pool::now_epoch_secs();
        warm_pool
            .upsert(PoolEntry::new(
                "ubuntu",
                "vm",
                "ssh",
                "/srv/warm",
                "abc",
                now + 600.0,
                now,
            ))
            .expect("warm entry");
        let dispatcher = FakeDispatcher::new(TargetStatus::Fail);
        let request = ship_request(vec![target]);

        let outcome = execute_ship(
            &request,
            ShipStores {
                queue: &mut queue,
                evidence: &evidence,
                ship_state: &ship_state,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: temp.path(),
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect("execute");

        assert!(!outcome.job.passed());
        assert_eq!(
            dispatcher.seen_workdirs.borrow()[0].as_deref(),
            Some("/srv/warm")
        );
        assert_eq!(
            dispatcher.seen_resume.borrow()[0].as_deref(),
            Some("configure")
        );
        assert!(
            warm_pool
                .get("ubuntu", "vm", crate::warm_pool::now_epoch_secs())
                .is_none()
        );
        let state = ship_state.get(42).expect("state");
        assert_eq!(state.evidence_snapshot["ubuntu"], "fail");
        assert_eq!(state.get_run("ubuntu").expect("run").status, "failed");
    }

    #[test]
    fn execute_ship_refuses_existing_state_sha_drift() {
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        ship_state
            .save(&ShipState::new(
                42,
                "danielraffel/pulp",
                "feature/test",
                "main",
                "old",
                "policy",
            ))
            .expect("save");
        let dispatcher = FakeDispatcher::new(TargetStatus::Pass);
        let request = ship_request(vec![target]);

        let error = execute_ship(
            &request,
            ShipStores {
                queue: &mut queue,
                evidence: &evidence,
                ship_state: &ship_state,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: temp.path(),
                config: &empty_config(temp.path()),
            },
            &dispatcher,
        )
        .expect_err("sha drift");

        assert!(matches!(
            error,
            ShipExecutionError::ShaDrift { existing, current }
                if existing == "old" && current == "abc"
        ));
        assert!(queue.get_pending().expect("pending").is_empty());
    }

    #[test]
    fn adopt_head_tolerates_sha_drift_but_still_enforces_policy() {
        // #346: --adopt-head relaxes ONLY the SHA-drift guard (so an amend /
        // force-push re-validates instead of dead-ending), and never relaxes
        // the policy-signature guard — a changed merge policy must still fail.
        let state = ShipState::new(
            42,
            "danielraffel/pulp",
            "feature/test",
            "main",
            "old",
            "policy",
        );

        // Without the flag, SHA drift is rejected.
        assert!(matches!(
            super::validate_existing_state(&state, "new", "main", "policy", false),
            Err(ShipExecutionError::ShaDrift { .. })
        ));
        // With the flag, the same SHA drift is tolerated...
        assert!(super::validate_existing_state(&state, "new", "main", "policy", true).is_ok());
        // Base drift is also validation-identity drift and requires adoption.
        assert!(matches!(
            super::validate_existing_state(&state, "old", "release", "policy", false),
            Err(ShipExecutionError::BaseDrift { .. })
        ));
        assert!(super::validate_existing_state(&state, "old", "release", "policy", true).is_ok());
        // ...but a policy-signature change is STILL rejected even with the flag.
        assert!(matches!(
            super::validate_existing_state(&state, "new", "main", "different-policy", true),
            Err(ShipExecutionError::PolicyDrift { .. })
        ));
    }

    #[test]
    fn adopt_head_reconciles_drift_clearing_stale_evidence() {
        // #346: adopting the new head must clear prior remote runs + evidence so
        // the new head re-validates from scratch — never bless stale validation.
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let mut seeded = ShipState::new(
            42,
            "danielraffel/pulp",
            "feature/test",
            "main",
            "old",
            "policy",
        );
        seeded
            .evidence_snapshot
            .insert("ssh".to_owned(), "passed-on-old-head".to_owned());
        let old_attempt = chrono::Utc::now();
        seeded.merge_queue_attempt_started_at = Some(old_attempt);
        seeded.merge_queue_observed_at = Some(old_attempt);
        seeded.merge_queue_enqueue_succeeded_at = Some(old_attempt);
        store.save(&seeded).expect("save");

        // Build a request whose policy matches the seeded state so only SHA
        // drift is in play, with adopt_head set and the live SHA = "abc".
        let mut request = ship_request(vec![target]);
        request.adopt_head = true;
        request.base_branch = "release".to_owned();
        let target_names = vec![request.targets[0].name.clone()];
        let mut seeded = store.get(42).expect("seeded present");
        seeded.policy_signature =
            super::policy_signature(&request.targets, &target_names, request.mode);
        store.save(&seeded).expect("re-save with matching policy");

        let reconciled = super::load_or_create_state(&request, &target_names, &store, None)
            .expect("adopt-head reconciles drift");
        assert_eq!(reconciled.head_sha, "abc", "adopts the current head");
        assert_eq!(
            reconciled.base_branch, "release",
            "adopts the current validated base"
        );
        assert!(
            reconciled.evidence_snapshot.is_empty(),
            "stale evidence cleared so the new head re-validates"
        );
        assert!(
            reconciled.merge_queue_attempt_started_at > Some(old_attempt),
            "adopted head gets a fresh queue authority epoch"
        );
        assert_eq!(reconciled.merge_queue_observed_at, None);
        assert_eq!(reconciled.merge_queue_enqueue_succeeded_at, None);
    }

    #[test]
    fn reship_clears_a_prior_orphan_abandonment() {
        // A re-ship is the intended recovery from an opt-in orphan abandonment:
        // reusing the existing state for a new execution must clear the terminal
        // marker, or the re-shipped PR would be short-circuited to failure.
        let target = ssh_target();
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let request = ship_request(vec![target]);
        let target_names = vec![request.targets[0].name.clone()];
        let mut seeded = ShipState::new(
            request.pr,
            &request.repo,
            &request.branch,
            &request.base_branch,
            &request.sha,
            super::policy_signature(&request.targets, &target_names, request.mode),
        );
        seeded.mark_abandoned(AbandonRecord {
            reason: "orphaned".to_owned(),
            evidence: "queue_stale".to_owned(),
            stalled_minutes: 90,
            job_id: Some("job-1".to_owned()),
            abandoned_at: Utc::now(),
        });
        store.save(&seeded).expect("save abandoned state");

        let reused = super::load_or_create_state(&request, &target_names, &store, None)
            .expect("reship reuses the state");
        assert!(
            !reused.is_abandoned(),
            "beginning a ship execution clears the terminal abandonment marker"
        );
    }

    /// Dispatcher that returns a scripted `(status, failure_class)` per attempt
    /// (last entry repeats), writes a distinct log file per attempt so evidence
    /// preservation is observable, and can cancel the durable job after a chosen
    /// attempt to exercise the retry loop's cancellation handling. `Mutex`-backed
    /// so it satisfies the `Sync` bound the ship entrypoints require.
    struct SequenceDispatcher {
        steps: Vec<(TargetStatus, Option<&'static str>)>,
        cancel_after_attempt: Option<usize>,
        seen_log_paths: Mutex<Vec<PathBuf>>,
    }

    impl SequenceDispatcher {
        fn new(steps: Vec<(TargetStatus, Option<&'static str>)>) -> Self {
            Self {
                steps,
                cancel_after_attempt: None,
                seen_log_paths: Mutex::new(Vec::new()),
            }
        }

        fn cancelling_after(mut self, attempt: usize) -> Self {
            self.cancel_after_attempt = Some(attempt);
            self
        }

        fn call_count(&self) -> usize {
            self.seen_log_paths.lock().expect("seen lock").len()
        }
    }

    impl ShipTargetDispatcher for SequenceDispatcher {
        fn validate(&self, request: DispatchValidationRequest<'_, '_>) -> TargetResult {
            let index = {
                let mut seen = self.seen_log_paths.lock().expect("seen lock");
                let idx = seen.len();
                seen.push(request.log_path.clone());
                idx
            };
            if let Some(parent) = request.log_path.parent() {
                std::fs::create_dir_all(parent).expect("log dir");
            }
            std::fs::write(&request.log_path, format!("attempt {index}\n")).expect("log write");
            let (status, failure_class) = self
                .steps
                .get(index)
                .or_else(|| self.steps.last())
                .copied()
                .expect("at least one step");
            let now = Utc::now();
            let mut result = TargetResult::new(
                request.target.name.clone(),
                request.target.platform.clone(),
                status,
                request.target.backend_name.clone(),
            );
            result.started_at = Some(now);
            result.completed_at = Some(now);
            result.log_path = Some(request.log_path.to_string_lossy().into_owned());
            result.failure_class = failure_class.map(str::to_owned);
            if self.cancel_after_attempt == Some(index) {
                cancel_job_from_log_path(&request.log_path);
            }
            result
        }
    }

    fn drive_targets(
        target: ResolvedTarget,
        dispatcher: &(impl ShipTargetDispatcher + Sync),
        policy: crate::ship_retry::TransientRetryPolicy,
    ) -> (tempfile::TempDir, Job, PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let target_name = target.name.clone();
        let request = ship_request(vec![target]);
        let job = submit_ship(&request, &mut queue, temp.path(), &state_dir).expect("submit");
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("held");
        let started = queue
            .start_pending_jobs_for_drain(&drain_lock, std::slice::from_ref(&job.id))
            .expect("start")
            .pop()
            .expect("started");
        let base_log = target_log_path(&state_dir, &started.id, &target_name);
        let outcome = execute_targets_with_options(
            &request,
            &state_dir,
            &mut queue,
            &warm_pool,
            dispatcher,
            started,
            TargetExecOptions {
                cwd: temp.path(),
                defer_host_pool_lease_unavailable: false,
                reclassify_vitals_path: None,
                transient_retry: policy,
                config: None,
            },
        )
        .expect("targets");
        let job = match outcome {
            TargetExecutionOutcome::Completed(job) => job,
            other @ TargetExecutionOutcome::Deferred { .. } => {
                panic!("expected completed, got {other:?}")
            }
        };
        (temp, job, base_log)
    }

    fn only_result(job: &Job) -> &TargetResult {
        job.results.values().next().expect("one target result")
    }

    #[test]
    fn transient_retry_disabled_runs_single_attempt() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = local_target("mac", "macos-arm64", temp.path());
        let dispatcher = SequenceDispatcher::new(vec![(TargetStatus::Fail, Some("INFRA"))]);

        let (_temp, job, base_log) = drive_targets(
            target,
            &dispatcher,
            crate::ship_retry::TransientRetryPolicy::disabled(),
        );

        // Default policy = exactly one attempt, byte-identical to no-retry.
        assert_eq!(dispatcher.call_count(), 1);
        let result = only_result(&job);
        assert_eq!(result.status, TargetStatus::Fail);
        assert!(
            result.error_message.is_none(),
            "no retry note when disabled"
        );
        assert!(result.phase.is_none());
        assert!(
            !retry_attempt_log_path(&base_log, 1).exists(),
            "no retry log written"
        );
    }

    #[test]
    fn transient_local_infra_recovers_on_retry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = local_target("mac", "macos-arm64", temp.path());
        let dispatcher = SequenceDispatcher::new(vec![
            (TargetStatus::Fail, Some("INFRA")),
            (TargetStatus::Pass, None),
        ]);

        let (_temp, job, base_log) = drive_targets(
            target,
            &dispatcher,
            crate::ship_retry::TransientRetryPolicy::with_max_retries(1),
        );

        assert_eq!(dispatcher.call_count(), 2, "one retry after the INFRA blip");
        let result = only_result(&job);
        assert!(result.passed(), "recovered on the retry");
        // A recovered result records the retry in `phase`, never `error_message`
        // (a non-empty message is read elsewhere as a failure signal).
        assert!(result.error_message.is_none());
        assert!(
            result
                .phase
                .as_deref()
                .unwrap_or_default()
                .contains("recovered"),
            "phase notes the recovery: {:?}",
            result.phase
        );
        // Both attempts' logs are preserved under distinct paths.
        assert!(base_log.exists(), "attempt-0 log preserved");
        let retry_log = retry_attempt_log_path(&base_log, 1);
        assert!(retry_log.exists(), "retry log written to a distinct path");
        assert_ne!(base_log, retry_log);
        assert_eq!(
            std::fs::read_to_string(&base_log).expect("read base"),
            "attempt 0\n"
        );
        assert_eq!(
            std::fs::read_to_string(&retry_log).expect("read retry"),
            "attempt 1\n"
        );
    }

    #[test]
    fn transient_local_infra_exhausts_retries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = local_target("mac", "macos-arm64", temp.path());
        let dispatcher = SequenceDispatcher::new(vec![(TargetStatus::Fail, Some("INFRA"))]);

        let (_temp, job, _base_log) = drive_targets(
            target,
            &dispatcher,
            crate::ship_retry::TransientRetryPolicy::with_max_retries(1),
        );

        assert_eq!(dispatcher.call_count(), 2, "first attempt + one retry");
        let result = only_result(&job);
        assert_eq!(result.status, TargetStatus::Fail);
        assert!(
            result
                .error_message
                .as_deref()
                .unwrap_or_default()
                .contains("transient retry exhausted"),
            "failure note records the exhausted retry: {:?}",
            result.error_message
        );
    }

    #[test]
    fn transient_local_test_failure_is_not_retried() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = local_target("mac", "macos-arm64", temp.path());
        // An authoritative TEST failure must never be masked behind a retry.
        let dispatcher = SequenceDispatcher::new(vec![(TargetStatus::Fail, Some("TEST"))]);

        let (_temp, job, _base_log) = drive_targets(
            target,
            &dispatcher,
            crate::ship_retry::TransientRetryPolicy::with_max_retries(2),
        );

        assert_eq!(
            dispatcher.call_count(),
            1,
            "TEST is authoritative, no retry"
        );
        let result = only_result(&job);
        assert_eq!(result.status, TargetStatus::Fail);
        assert!(result.error_message.is_none());
    }

    #[test]
    fn transient_local_contract_failure_is_not_retried() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = local_target("mac", "macos-arm64", temp.path());
        let dispatcher = SequenceDispatcher::new(vec![(TargetStatus::Fail, Some("CONTRACT"))]);

        let (_temp, job, _base_log) = drive_targets(
            target,
            &dispatcher,
            crate::ship_retry::TransientRetryPolicy::with_max_retries(2),
        );

        assert_eq!(dispatcher.call_count(), 1, "CONTRACT is never retried");
        assert_eq!(only_result(&job).status, TargetStatus::Fail);
    }

    #[test]
    fn transient_local_timeout_is_not_retried_same_leg() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = local_target("mac", "macos-arm64", temp.path());
        // Same-backend retry is stricter than the global taxonomy: a local
        // TIMEOUT would just re-burn the wall-clock budget, so it is not re-run.
        let dispatcher = SequenceDispatcher::new(vec![(TargetStatus::Fail, Some("TIMEOUT"))]);

        let (_temp, job, _base_log) = drive_targets(
            target,
            &dispatcher,
            crate::ship_retry::TransientRetryPolicy::with_max_retries(2),
        );

        assert_eq!(dispatcher.call_count(), 1, "local TIMEOUT is not re-run");
        assert_eq!(only_result(&job).status, TargetStatus::Fail);
    }

    #[test]
    fn transient_remote_infra_is_not_retried_same_leg() {
        // A non-local backend already has next-backend failover; same-leg retry
        // is local-only.
        let dispatcher = SequenceDispatcher::new(vec![(TargetStatus::Fail, Some("INFRA"))]);

        let (_temp, job, _base_log) = drive_targets(
            ssh_target(),
            &dispatcher,
            crate::ship_retry::TransientRetryPolicy::with_max_retries(2),
        );

        assert_eq!(
            dispatcher.call_count(),
            1,
            "remote INFRA is not re-run in place"
        );
        assert_eq!(only_result(&job).status, TargetStatus::Fail);
    }

    #[test]
    fn transient_retry_honors_cancellation_between_attempts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = local_target("mac", "macos-arm64", temp.path());
        // Cancel the durable job during attempt 0; the retry loop must stop
        // rather than spend another attempt.
        let dispatcher =
            SequenceDispatcher::new(vec![(TargetStatus::Fail, Some("INFRA"))]).cancelling_after(0);

        let (_temp, job, _base_log) = drive_targets(
            target,
            &dispatcher,
            crate::ship_retry::TransientRetryPolicy::with_max_retries(2),
        );

        assert_eq!(dispatcher.call_count(), 1, "no retry after cancellation");
        assert_eq!(job.status, JobStatus::Cancelled);
    }

    #[test]
    fn transient_retry_never_touches_scheduler_deferred() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = local_target("mac", "macos-arm64", temp.path());
        // A scheduler-deferred result must stay on the defer path, never retried.
        let dispatcher = FakeDispatcher::new(TargetStatus::Pending)
            .with_scheduler_defer("host_pool_lease_unavailable");
        let state_dir = temp.path().join("state");
        let mut queue = Queue::new(&state_dir).expect("queue");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let request = ship_request(vec![target]);
        let job = submit_ship(&request, &mut queue, temp.path(), &state_dir).expect("submit");
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("held");
        let started = queue
            .start_pending_jobs_for_drain(&drain_lock, std::slice::from_ref(&job.id))
            .expect("start")
            .pop()
            .expect("started");

        let outcome = execute_targets_with_options(
            &request,
            &state_dir,
            &mut queue,
            &warm_pool,
            &dispatcher,
            started,
            TargetExecOptions {
                cwd: temp.path(),
                defer_host_pool_lease_unavailable: true,
                reclassify_vitals_path: None,
                transient_retry: crate::ship_retry::TransientRetryPolicy::with_max_retries(2),
                config: None,
            },
        )
        .expect("targets");

        match outcome {
            TargetExecutionOutcome::Deferred { reason, .. } => {
                assert_eq!(reason, "host_pool_lease_unavailable");
            }
            other @ TargetExecutionOutcome::Completed(_) => {
                panic!("expected scheduler deferral, got {other:?}")
            }
        }
    }

    #[test]
    fn execute_ship_retries_transient_local_infra_from_config() {
        // End-to-end through the real ship entrypoint: config opt-in →
        // resolved policy → same-backend retry → recovered pass.
        let temp = tempfile::tempdir().expect("tempdir");
        let target = local_target("mac", "macos-arm64", temp.path());
        let mut queue = Queue::new(temp.path().join("state")).expect("queue");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let ship_state = ShipStateStore::new(temp.path().join("ship")).expect("ship");
        let warm_pool = WarmPool::new(temp.path().join("warm_pool.json"));
        let dispatcher = SequenceDispatcher::new(vec![
            (TargetStatus::Fail, Some("INFRA")),
            (TargetStatus::Pass, None),
        ]);
        let request = ship_request(vec![target]);
        let config = LoadedConfig {
            data: table("[ship]\ntransient_local_retries = 1\n"),
            global_dir: temp.path().join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        };

        let outcome = execute_ship(
            &request,
            ShipStores {
                queue: &mut queue,
                evidence: &evidence,
                ship_state: &ship_state,
                warm_pool: &warm_pool,
                cwd: temp.path(),
                state_dir: temp.path(),
                config: &config,
            },
            &dispatcher,
        )
        .expect("execute");

        assert_eq!(dispatcher.call_count(), 2, "config opt-in drove one retry");
        assert!(
            outcome.job.passed(),
            "recovered through the ship entrypoint"
        );
    }
}
