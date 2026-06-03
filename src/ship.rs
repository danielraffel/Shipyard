//! Ship execution orchestration helpers.
//!
//! The full `ship` command eventually ties together dispatch, queue,
//! evidence, ship-state, and merge behavior. This module starts with
//! the warm-pool and durable execution logic so executor wiring can
//! reuse it without embedding policy decisions in CLI code.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::evidence::{EvidenceRecord, EvidenceStore};
use crate::executor::dispatch::{
    DispatchValidationRequest, ExecutorDispatcher, ResolvedBackend, ResolvedHostPoolConfig,
    ResolvedHostPoolMember, ResolvedTarget,
};
use crate::executor::streaming::ProgressEvent;
use crate::host_pool::{
    HostPoolConfig, HostPoolLeaseStore, HostPoolMemberConfig, default_lease_path,
};
use crate::job::{
    DEFAULT_RUNNING_JOB_STALE_SECONDS, Job, JobKind, JobStatus, JobTransitionError, Priority,
    TargetResult, TargetStatus, ValidationMode,
};
use crate::queue::{Queue, QueueDeferredRequeue, QueueError, STALE_RUNNING_CANCEL_REASON};
use crate::queue_request::{
    QueueOutcomeStore, QueueRequestError, QueueRequestStore, QueuedExecutionEnvelope,
    QueuedExecutionKind, QueuedExecutionOutcome,
};
use crate::queue_scheduler::{apply_admit_pass_for_drain, plan_admit_pass_from_jobs};
use crate::ship_state::{
    DispatchedRun, ShipState, ShipStatePrLock, ShipStateStore, compute_policy_signature,
};
use crate::warm_pool::{
    PoolEntry, WarmPool, compute_expires_at, is_backend_eligible, warm_host_key,
};

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
            Self::PolicyDrift { existing, current } => write!(
                formatter,
                "ship state policy drift: existing {existing}, current {current}"
            ),
            Self::JobTransition(error) => write!(formatter, "{error}"),
            Self::Queue(error) => write!(formatter, "{error}"),
            Self::QueueRequest(error) => write!(formatter, "{error}"),
            Self::Evidence(error) => write!(formatter, "evidence write failed: {error}"),
            Self::ShipState(error) => write!(formatter, "ship-state write failed: {error}"),
            Self::WarmPool(error) => write!(formatter, "warm-pool write failed: {error}"),
            Self::SchedulerDeferred(reason) => {
                write!(formatter, "scheduler deferred validation: {reason}")
            }
            Self::HostPool(error) => write!(formatter, "host-pool scheduler read failed: {error}"),
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
            | Self::PolicyDrift { .. }
            | Self::Evidence(_)
            | Self::ShipState(_)
            | Self::SchedulerDeferred(_)
            | Self::HostPool(_)
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
    refuse_same_pr_running_ship(queue, state_dir, request)?;
    let target_names = target_names(&request.targets);
    let job = Job::create(
        request.sha.clone(),
        request.branch.clone(),
        target_names,
        request.mode,
        request.priority,
    )
    .with_kind(JobKind::Ship);
    QueueRequestStore::new(state_dir)
        .map_err(QueueRequestError::from)?
        .save(&QueuedExecutionEnvelope::from_ship_request(
            job.id.clone(),
            cwd,
            request,
        ))?;
    queue.enqueue(job.clone())?;
    Ok(job)
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
    drain_or_wait_ship_with_options(
        request,
        job,
        stores,
        dispatcher,
        CooperativeDrainOptions::default(),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn drain_or_wait_ship_with_options<D: ShipTargetDispatcher + Sync>(
    request: &ShipExecutionRequest,
    job: Job,
    stores: ShipStores<'_>,
    dispatcher: &D,
    options: CooperativeDrainOptions,
) -> Result<ShipExecutionOutcome, ShipExecutionError> {
    let ShipStores {
        queue,
        evidence,
        ship_state,
        warm_pool,
        cwd,
        state_dir,
    } = stores;
    let mut wait_iterations = 0usize;
    loop {
        if let Some(outcome) = terminal_ship_outcome(queue, state_dir, request, &job.id)? {
            return Ok(outcome);
        }
        if let Some(drain_lock) = queue.acquire_drain_lock()? {
            let recovered = queue.recover_stale_running_jobs_for_drain(&drain_lock)?;
            persist_recovered_outcomes(&recovered, state_dir, ship_state)?;
            if let Some(outcome) = terminal_ship_outcome(queue, state_dir, request, &job.id)? {
                return Ok(outcome);
            }
            run_drain_worker_cycle(
                queue,
                &drain_lock,
                evidence,
                ship_state,
                warm_pool,
                cwd,
                state_dir,
                dispatcher,
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
        state_dir,
        ..
    } = stores;
    if let Some(cancelled) = durable_cancelled_job(queue, &job)? {
        return Ok(ShipExecutionOutcome {
            job: cancelled,
            ship_state: unsaved_ship_state(request, &job.target_names),
            resumed_existing_state: false,
        });
    }
    let ship_state_lock = ship_state
        .lock_pr(request.pr)
        .map_err(|error| ShipExecutionError::ShipState(error.to_string()))?;
    let resumed_existing_state = ship_state
        .get_locked(request.pr, &ship_state_lock)
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
    if let Err(error) = ship_state.save_locked(&state, &ship_state_lock) {
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
        defer_host_pool_lease_unavailable,
    )?
    .into_completed()?;
    if job.status == JobStatus::Cancelled {
        QueueOutcomeStore::new(state_dir)
            .map_err(QueueRequestError::from)?
            .save(&QueuedExecutionOutcome::ship(
                job.id.clone(),
                request.pr,
                state.clone(),
                resumed_existing_state,
            ))?;
        return Ok(ShipExecutionOutcome {
            job,
            ship_state: state,
            resumed_existing_state,
        });
    }
    job = job.complete()?;
    record_evidence(evidence, request, &job)?;
    update_ship_state_from_job(&mut state, request, &job);
    ship_state
        .save_locked(&state, &ship_state_lock)
        .map_err(|error| ShipExecutionError::ShipState(error.to_string()))?;
    QueueOutcomeStore::new(state_dir)
        .map_err(QueueRequestError::from)?
        .save(&QueuedExecutionOutcome::ship(
            job.id.clone(),
            request.pr,
            state.clone(),
            resumed_existing_state,
        ))?;
    queue.update(&job)?;

    Ok(ShipExecutionOutcome {
        job,
        ship_state: state,
        resumed_existing_state,
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
    let target_names = target_names(&request.targets);
    let job = Job::create(
        request.sha.clone(),
        request.branch.clone(),
        target_names,
        request.mode,
        request.priority,
    )
    .with_kind(JobKind::Run);
    QueueRequestStore::new(state_dir)
        .map_err(QueueRequestError::from)?
        .save(&QueuedExecutionEnvelope::from_run_request(
            job.id.clone(),
            cwd,
            request,
        ))?;
    queue.enqueue(job.clone())?;
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
    } = stores;
    let mut wait_iterations = 0usize;
    loop {
        if let Some(outcome) = terminal_run_outcome(queue, state_dir, &job.id)? {
            return Ok(outcome);
        }
        if let Some(drain_lock) = queue.acquire_drain_lock()? {
            let ship_state = ShipStateStore::new(state_dir.join("ship"))
                .map_err(|error| ShipExecutionError::ShipState(error.to_string()))?;
            let recovered = queue.recover_stale_running_jobs_for_drain(&drain_lock)?;
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
                dispatcher,
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
        state_dir,
        ..
    } = stores;
    if let Some(cancelled) = durable_cancelled_job(queue, &job)? {
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
        defer_host_pool_lease_unavailable,
    )?
    .into_completed()?;
    if job.status == JobStatus::Cancelled {
        QueueOutcomeStore::new(state_dir)
            .map_err(QueueRequestError::from)?
            .save(&QueuedExecutionOutcome::run(job.id.clone()))?;
        return Ok(RunExecutionOutcome { job });
    }
    job = job.complete()?;
    record_evidence(evidence, &shim, &job)?;
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
    if durable.status == JobStatus::Cancelled {
        Ok(Some(durable))
    } else {
        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
fn run_drain_worker_cycle<D: ShipTargetDispatcher + Sync>(
    queue: &mut Queue,
    drain_lock: &crate::queue::DrainLock,
    evidence: &EvidenceStore,
    ship_state: &ShipStateStore,
    warm_pool: &WarmPool,
    cwd: &Path,
    state_dir: &Path,
    dispatcher: &D,
) -> Result<(), ShipExecutionError> {
    let request_store = QueueRequestStore::new(state_dir).map_err(QueueRequestError::from)?;
    let outcome_store = QueueOutcomeStore::new(state_dir).map_err(QueueRequestError::from)?;
    let _trimmed_job_ids = queue.trim_terminal_jobs_for_drain(drain_lock)?;
    let jobs = queue.get_all()?;
    sweep_absent_queue_envelopes(&jobs, &request_store, &outcome_store)?;
    let pools = scheduler_host_pools(&jobs, &request_store)?;
    let leases = HostPoolLeaseStore::new(default_lease_path(state_dir))
        .leases()
        .map_err(|error| ShipExecutionError::HostPool(error.to_string()))?;
    let mut pass = plan_admit_pass_from_jobs(&jobs, &request_store, &pools, &leases, Utc::now());
    cap_admit_pass_workers(&jobs, &mut pass, DEFAULT_DRAIN_MAX_WORKERS);
    let applied = apply_admit_pass_for_drain(queue, drain_lock, &pass)?;
    if applied.started.is_empty() {
        return Ok(());
    }

    let worker_inputs = applied
        .started
        .into_iter()
        .map(|job| {
            let envelope = request_store
                .load(&job.id)?
                .ok_or_else(|| ShipExecutionError::MissingQueuedJob(job.id.clone()))?;
            Ok((job, envelope))
        })
        .collect::<Result<Vec<_>, ShipExecutionError>>()?;

    let queue_state_dir = queue.state_dir().to_path_buf();
    let mut worker_results = Vec::new();
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for (job, envelope) in worker_inputs {
            let evidence = evidence.clone();
            let ship_state = ship_state.clone();
            let warm_pool = warm_pool.clone();
            let state_dir = state_dir.to_path_buf();
            let queue_state_dir = queue_state_dir.clone();
            let fallback_cwd = cwd.to_path_buf();
            handles.push((
                job.id.clone(),
                scope.spawn(move || {
                    run_started_worker(
                        job,
                        envelope,
                        &evidence,
                        &ship_state,
                        &warm_pool,
                        &fallback_cwd,
                        &queue_state_dir,
                        &state_dir,
                        dispatcher,
                    )
                }),
            ));
        }
        for (job_id, handle) in handles {
            let result = match handle.join() {
                Ok(result) => result,
                Err(_) => Err(ShipExecutionError::WorkerJoin(job_id.clone())),
            };
            worker_results.push((job_id, result));
        }
    });

    let requeues = worker_results
        .into_iter()
        .filter_map(|(job_id, result)| match result {
            Ok(()) => None,
            Err(ShipExecutionError::SchedulerDeferred(reason)) => Some(Ok(QueueDeferredRequeue {
                job_id,
                reason,
                defer_until: Some(defer_until(Utc::now())),
            })),
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>, ShipExecutionError>>()?;
    if !requeues.is_empty() {
        queue.requeue_deferred_running_jobs_for_drain(drain_lock, &requeues)?;
    }
    Ok(())
}

fn sweep_absent_queue_envelopes(
    jobs: &[Job],
    request_store: &QueueRequestStore,
    outcome_store: &QueueOutcomeStore,
) -> Result<(), ShipExecutionError> {
    let active_job_ids = jobs
        .iter()
        .map(|job| job.id.clone())
        .collect::<BTreeSet<_>>();
    request_store.sweep_absent_older_than(&active_job_ids, QUEUE_ENVELOPE_SWEEP_GRACE)?;
    outcome_store.sweep_absent_older_than(&active_job_ids, QUEUE_ENVELOPE_SWEEP_GRACE)?;
    Ok(())
}

fn persist_recovered_outcomes(
    recovered: &[Job],
    state_dir: &Path,
    ship_state: &ShipStateStore,
) -> Result<(), ShipExecutionError> {
    if recovered.is_empty() {
        return Ok(());
    }
    let request_store = QueueRequestStore::new(state_dir).map_err(QueueRequestError::from)?;
    let outcome_store = QueueOutcomeStore::new(state_dir).map_err(QueueRequestError::from)?;
    for job in recovered {
        let Some(envelope) = request_store.load(&job.id)? else {
            continue;
        };
        match envelope.kind {
            QueuedExecutionKind::Run => {
                outcome_store.save(&QueuedExecutionOutcome::run(job.id.clone()))?;
            }
            QueuedExecutionKind::Ship => {
                let request = envelope.to_ship_request()?;
                let existing = ship_state.get(request.pr);
                let resumed_existing_state = existing.is_some();
                let state =
                    existing.unwrap_or_else(|| unsaved_ship_state(&request, &job.target_names));
                outcome_store.save(&QueuedExecutionOutcome::ship(
                    job.id.clone(),
                    request.pr,
                    state,
                    resumed_existing_state,
                ))?;
            }
        }
    }
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
fn run_started_worker<D: ShipTargetDispatcher>(
    job: Job,
    envelope: QueuedExecutionEnvelope,
    evidence: &EvidenceStore,
    ship_state: &ShipStateStore,
    warm_pool: &WarmPool,
    fallback_cwd: &Path,
    queue_state_dir: &Path,
    state_dir: &Path,
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
                },
                dispatcher,
                true,
            )?;
        }
    }
    Ok(())
}

fn defer_until(now: DateTime<Utc>) -> DateTime<Utc> {
    now + chrono::Duration::seconds(5)
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
        let QueuedExecutionEnvelope {
            request: crate::queue_request::QueuedExecutionRequest::Ship(existing),
            ..
        } = envelope
        else {
            continue;
        };
        if existing.repo != request.repo || existing.pr != request.pr {
            continue;
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
fn execute_targets_with_options<D: ShipTargetDispatcher>(
    request: &ShipExecutionRequest,
    state_dir: &Path,
    queue: &mut Queue,
    warm_pool: &WarmPool,
    dispatcher: &D,
    mut job: Job,
    defer_host_pool_lease_unavailable: bool,
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
        let log_path = target_log_path(state_dir, &job.id, &target.name);
        let decision = apply_warm_reuse(
            warm_pool,
            target,
            &request.sha,
            request.resume_from.as_deref(),
            request.warm_disabled,
            crate::warm_pool::now_epoch_secs(),
        );
        job = job.with_result(running_result(&decision.target, &log_path, job.started_at));
        queue.update(&job)?;

        let dispatch_job_id = job.id.clone();
        let progress_log_path = log_path.clone();
        let mut progress_error = None;
        let mut progress_cancelled = None;
        let result = {
            let mut progress_callback = |event: ProgressEvent| {
                if progress_error.is_some() || progress_cancelled.is_some() {
                    return;
                }
                match durable_cancelled_job(queue, &job) {
                    Ok(Some(cancelled)) => {
                        progress_cancelled = Some(cancelled);
                        return;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        progress_error = Some(error);
                        return;
                    }
                }
                apply_progress_event(&mut job, &decision.target, &progress_log_path, event);
                if let Err(error) = queue.update(&job) {
                    progress_error = Some(ShipExecutionError::Queue(error));
                    return;
                }
                match durable_cancelled_job(queue, &job) {
                    Ok(Some(cancelled)) => {
                        progress_cancelled = Some(cancelled);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        progress_error = Some(error);
                    }
                }
            };
            dispatcher.validate(DispatchValidationRequest {
                job_id: Some(dispatch_job_id),
                defer_host_pool_lease_unavailable,
                sha: request.sha.clone(),
                branch: request.branch.clone(),
                target: &decision.target,
                log_path,
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
        if result.is_scheduler_deferred() {
            let reason = result
                .scheduler_defer_reason
                .clone()
                .unwrap_or_else(|| "scheduler_deferred".to_owned());
            return Ok(TargetExecutionOutcome::Deferred { job, reason });
        }
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
        || store.get(request.pr),
        |lock| store.get_locked(request.pr, lock),
    );
    if let Some(mut existing) = existing {
        validate_existing_state(&existing, &request.sha, &policy, request.adopt_head)?;
        if request.adopt_head && existing.is_sha_drift(&request.sha) {
            // Adopt the amended/force-pushed head. Clear prior remote runs and
            // evidence so the new head is re-validated from scratch — never
            // bless stale validation for a possibly-different tree. `head_sha`
            // also gates auto-merge's live-head preflight, so it must track the
            // SHA we actually validate (Shipyard #346; codex review). A
            // same-tree fast path that preserves evidence for a trailer-only
            // amend is a possible follow-up.
            existing.head_sha.clone_from(&request.sha);
            existing.dispatched_runs.clear();
            existing.evidence_snapshot.clear();
        }
        existing.commit_subject.clone_from(&request.commit_subject);
        refresh_pr_metadata(&mut existing, request);
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
    policy: &str,
    adopt_head: bool,
) -> Result<(), ShipExecutionError> {
    if !adopt_head && state.is_sha_drift(sha) {
        return Err(ShipExecutionError::ShaDrift {
            existing: state.head_sha.clone(),
            current: sha.to_owned(),
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

fn policy_signature(
    targets: &[ResolvedTarget],
    target_names: &[String],
    mode: ValidationMode,
) -> String {
    let platforms = targets
        .iter()
        .map(|target| target.platform.clone())
        .collect::<Vec<_>>();
    compute_policy_signature(&platforms, target_names, policy_mode_label(mode))
}

fn policy_mode_label(mode: ValidationMode) -> &'static str {
    match mode {
        ValidationMode::Full => "FULL",
        ValidationMode::Smoke => "SMOKE",
    }
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

fn record_evidence(
    evidence: &EvidenceStore,
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
            .record(&evidence_record(request, result, target))
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
        target_name: result.target_name.clone(),
        platform: result.platform.clone(),
        status: evidence_status(result).to_owned(),
        backend: result.backend.clone(),
        completed_at: result.completed_at.unwrap_or_else(Utc::now),
        duration_secs: result.duration_secs,
        host: target.and_then(|target| target.host.clone()),
        primary_backend: result.primary_backend.clone(),
        failover_reason: result.failover_reason.clone(),
        provider: result.provider.clone(),
        runner_profile: result.runner_profile.clone(),
        failure_class: result.failure_class.clone(),
        reused_from: result.reused_from.clone(),
        contract_digest: None,
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
    use std::sync::Mutex;
    use std::time::Duration as StdDuration;

    use chrono::{Duration, Utc};
    use toml::Table;

    use super::{
        CooperativeDrainOptions, RunExecutionRequest, RunStores, ShipExecutionError,
        ShipExecutionRequest, ShipStores, ShipTargetDispatcher, WarmPoolUpdate, apply_warm_reuse,
        cap_admit_pass_workers, drain_or_wait_run, drain_or_wait_run_with_options, execute_run,
        execute_run_worker, execute_ship, execute_ship_worker, execute_targets_with_options,
        load_run_outcome, load_ship_outcome, submit_run, submit_ship, update_warm_pool_after_run,
    };
    use crate::evidence::EvidenceStore;
    use crate::executor::dispatch::{
        DispatchValidationRequest, ResolvedTarget, resolve_targets_from_table,
    };
    use crate::executor::streaming::ProgressEvent;
    use crate::job::{JobStatus, Priority, TargetResult, TargetStatus, ValidationMode};
    use crate::queue::Queue;
    use crate::queue_request::{
        QueueOutcomeStore, QueueRequestStore, QueuedExecutionOutcome, QueuedExecutionRequest,
    };
    use crate::queue_scheduler::{AdmitPassPlan, RequestBackedAdmitPass, SamePrShipAdmission};
    use crate::ship_state::{ShipState, ShipStateStore};
    use crate::warm_pool::{PoolEntry, WarmPool};

    fn table(input: &str) -> Table {
        input.parse::<Table>().expect("valid TOML")
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
            targets,
        }
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
                    callback(event);
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
            .get_target("feature/test", "ubuntu")
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
            true,
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

        let error = submit_ship(
            &ship_request(vec![target]),
            &mut queue,
            temp.path(),
            &state_dir,
        )
        .expect_err("same PR running");

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
            super::validate_existing_state(&state, "new", "policy", false),
            Err(ShipExecutionError::ShaDrift { .. })
        ));
        // With the flag, the same SHA drift is tolerated...
        assert!(super::validate_existing_state(&state, "new", "policy", true).is_ok());
        // ...but a policy-signature change is STILL rejected even with the flag.
        assert!(matches!(
            super::validate_existing_state(&state, "new", "different-policy", true),
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
        store.save(&seeded).expect("save");

        // Build a request whose policy matches the seeded state so only SHA
        // drift is in play, with adopt_head set and the live SHA = "abc".
        let mut request = ship_request(vec![target]);
        request.adopt_head = true;
        let target_names = vec![request.targets[0].name.clone()];
        let mut seeded = store.get(42).expect("seeded present");
        seeded.policy_signature =
            super::policy_signature(&request.targets, &target_names, request.mode);
        store.save(&seeded).expect("re-save with matching policy");

        let reconciled = super::load_or_create_state(&request, &target_names, &store, None)
            .expect("adopt-head reconciles drift");
        assert_eq!(reconciled.head_sha, "abc", "adopts the current head");
        assert!(
            reconciled.evidence_snapshot.is_empty(),
            "stale evidence cleared so the new head re-validates"
        );
    }
}
