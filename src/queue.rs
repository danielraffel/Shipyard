//! Durable machine-global job queue.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::job::{CancellationProof, Job, JobKind, JobStatus, TargetResult, TargetStatus};
use crate::queue_request::{
    QueueRequestStore, QueuedExecutionEnvelope, QueuedExecutionKind, QueuedExecutionRequest,
};

/// Number of completed jobs retained in the durable queue.
pub const KEEP_COMPLETED: usize = 25;
/// Default number of Windows replace attempts after PR `#214`.
pub const WINDOWS_REPLACE_ATTEMPTS: usize = 18;
/// Base backoff delay. Attempt `n` sleeps in `[0.5*base*n, 1.5*base*n]`.
pub const WINDOWS_REPLACE_BASE_DELAY: Duration = Duration::from_millis(50);
const STALE_RECOVERY_MESSAGE: &str = "Process died mid-validation; job recovered on startup";
/// Reason recorded when a `Running` job is reaped because its worker went
/// silent past the heartbeat-staleness threshold (e.g. the worker process was
/// killed). Shared by the ship-time same-PR preflight and the drain admission
/// pass so the durable cancellation reads consistently wherever it originates.
pub const STALE_RUNNING_CANCEL_REASON: &str =
    "Running worker heartbeat stale; cancelled to unblock queued work";
const ORPHAN_REQUEST_MESSAGE: &str = "Queued request envelope missing or unreadable";
const SUPERSEDED_MESSAGE: &str =
    "Superseded by a newer queued job for the same branch, targets, and mode.";

/// Cancellation reason for a pending job whose pull request was already merged
/// while it waited in the queue.
pub const ALREADY_MERGED_CANCEL_REASON: &str = "PR already merged";

/// Fallible queue operation result.
pub type QueueResult<T> = Result<T, QueueError>;

/// Drain-owned pending job cancellation request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuePendingCancellation {
    /// Queue job id.
    pub job_id: String,
    /// Cancellation reason persisted on the job.
    pub reason: String,
    /// Authenticated typed cancellation authority, when one exists.
    pub proof: Option<CancellationProof>,
}

/// Drain-owned request to return a transiently deferred running job to pending.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueDeferredRequeue {
    /// Queue job id.
    pub job_id: String,
    /// Scheduler deferral reason persisted on the job.
    pub reason: String,
    /// Earliest retry time for the scheduler.
    pub defer_until: Option<DateTime<Utc>>,
}

/// Result of an idempotent recovery enqueue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryEnqueue {
    /// The exact recovery job was inserted.
    Inserted,
    /// The exact job was already present (for example after a crash following
    /// the queue commit but before the recovery receipt commit).
    Existing,
    /// Another queue job already owns the workload.
    OwnedBy(String),
}

/// Durable queue operation error.
#[derive(Debug)]
pub enum QueueError {
    /// Filesystem operation failed.
    Io(io::Error),
    /// Queue JSON serialization failed.
    Json(serde_json::Error),
    /// A stale writer attempted to overwrite a newer terminal or cancellation
    /// request state.
    StateConflict(String),
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "queue I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "queue JSON failed: {error}"),
            Self::StateConflict(reason) => write!(formatter, "queue state conflict: {reason}"),
        }
    }
}

impl std::error::Error for QueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::StateConflict(_) => None,
        }
    }
}

impl From<io::Error> for QueueError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for QueueError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Persistent, file-locked job queue.
#[derive(Debug)]
pub struct Queue {
    state_dir: PathBuf,
}

impl Queue {
    /// Open a queue rooted at `state_dir`.
    pub fn new(state_dir: impl Into<PathBuf>) -> io::Result<Self> {
        let state_dir = state_dir.into();
        crate::writer_domain_lease::ensure_protected_dir_all(&state_dir)?;
        Ok(Self { state_dir })
    }

    /// Queue state directory.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Durable queue file path for an application state directory.
    #[must_use]
    pub fn queue_file_at(state_dir: &Path) -> PathBuf {
        state_dir.join("queue.json")
    }

    /// Durable queue file path.
    #[must_use]
    pub fn queue_file(&self) -> PathBuf {
        Self::queue_file_at(&self.state_dir)
    }

    /// Drain lock file path.
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join("queue.lock")
    }

    /// Short-lived queue state lock file path.
    #[must_use]
    pub fn state_lock_file(&self) -> PathBuf {
        self.state_dir.join("queue.state.lock")
    }

    /// Acquire the short-lived ownership fence for one logical workload.
    ///
    /// Submitters and automatic recovery hold this only across durable
    /// request + queue admission. It is intentionally distinct from both the
    /// queue state lock and the long-lived ship execution lock.
    pub(crate) fn acquire_workload_admission_lock(
        &self,
        workload_scope: &str,
    ) -> QueueResult<WorkloadAdmissionLock> {
        let digest = format!("{:x}", Sha256::digest(workload_scope.as_bytes()));
        let path = self
            .state_dir
            .join("admission")
            .join(format!("{}.lock", &digest[..32]));
        Ok(WorkloadAdmissionLock {
            _state: StateLock::acquire(path)?,
        })
    }

    /// Add a job, superseding pending jobs for the same workload, branch,
    /// target list, and mode.
    pub fn enqueue(&mut self, job: Job) -> QueueResult<Job> {
        let request_store = QueueRequestStore::new(&self.state_dir).ok();
        self.with_jobs_locked(|jobs| {
            backfill_pending_workload_scopes(jobs, request_store.as_ref());
            cancel_superseded_pending(jobs, &job);
            jobs.push(job.clone());
            Ok(())
        })?;
        Ok(job)
    }

    /// Insert an exact recovery job only while its workload has no other
    /// durable owner. The check and insert share the queue state
    /// lock, closing the new-submitter race without creating a second
    /// scheduler.
    pub fn enqueue_recovery_if_unowned(
        &mut self,
        job: Job,
        ownership_cutoff: DateTime<Utc>,
    ) -> QueueResult<RecoveryEnqueue> {
        let request_store = QueueRequestStore::new(&self.state_dir).ok();
        self.with_jobs_locked_strict(|jobs| {
            backfill_recovery_owner_scopes(jobs, request_store.as_ref());
            if jobs.iter().any(|queued| queued.id == job.id) {
                return Ok(RecoveryEnqueue::Existing);
            }
            if let Some(owner) = jobs.iter().find(|queued| {
                let can_still_own =
                    matches!(queued.status, JobStatus::Pending | JobStatus::Running)
                        || queued.created_at > ownership_cutoff;
                can_still_own
                    && (same_workload_scope(queued, &job) || queued.workload_scope.is_none())
            }) {
                return Ok(RecoveryEnqueue::OwnedBy(owner.id.clone()));
            }
            jobs.push(job);
            Ok(RecoveryEnqueue::Inserted)
        })
    }

    /// Return the highest-priority pending job, preserving FIFO within each priority.
    pub fn next_pending(&mut self) -> QueueResult<Option<Job>> {
        let mut pending = self.get_pending()?;
        Ok(pending.drain(..).next())
    }

    /// Replace a queued job matched by id, then trim old completed jobs.
    pub fn update(&mut self, job: &Job) -> QueueResult<()> {
        let _lock = StateLock::acquire(self.state_lock_file())?;
        let mut jobs = self.read_jobs_from_disk()?;
        let Some(queued) = jobs.iter_mut().find(|queued| queued.id == job.id) else {
            return Err(QueueError::StateConflict(format!(
                "job {} is not present in the durable queue",
                job.id
            )));
        };
        if queued.status == JobStatus::Running
            && queued.cancel_requested_at.is_some()
            && (job.cancel_requested_at != queued.cancel_requested_at
                || job.cancellation_reason != queued.cancellation_reason
                || job.cancellation_proof != queued.cancellation_proof)
        {
            return Err(QueueError::StateConflict(format!(
                "job {} has a newer cancellation request",
                job.id
            )));
        }
        if matches!(queued.status, JobStatus::Completed | JobStatus::Cancelled)
            && queued.status != job.status
        {
            return Err(QueueError::StateConflict(format!(
                "job {} is already terminal as {:?}",
                job.id, queued.status
            )));
        }
        let terminal = matches!(job.status, JobStatus::Completed | JobStatus::Cancelled);
        if terminal {
            crate::log_retention::invalidate_conflicting_terminal_manifest(&self.state_dir, job)?;
        }
        *queued = job.clone();
        let _ = trim_terminal(&mut jobs);
        self.save_jobs_to_disk(&jobs)?;
        if terminal {
            // The queue outcome is authoritative and must not be stranded by
            // an ancillary manifest failure (notably a full log filesystem).
            // Missing manifests remain fail-safe protected during cleanup.
            let _ = crate::log_retention::write_terminal_manifest(&self.state_dir, job);
        }
        Ok(())
    }

    /// Hold the queue state lock across a retention mutation boundary.
    pub(crate) fn lock_for_log_cleanup(&self) -> QueueResult<QueueStateGuard> {
        Ok(QueueStateGuard {
            _inner: StateLock::acquire(self.state_lock_file())?,
        })
    }

    /// Publish a recovered terminal manifest only if its queue disposition is
    /// still current, serialized with all other queue/manifest mutations.
    pub(crate) fn publish_terminal_manifest_if_current(&self, recovered: &Job) -> QueueResult<()> {
        let _lock = StateLock::acquire(self.state_lock_file())?;
        let jobs = self.read_jobs_from_disk()?;
        let Some(current) = jobs.iter().find(|job| job.id == recovered.id) else {
            return Ok(());
        };
        let recovered_manifest = crate::log_retention::TerminalLogManifest::from_job(recovered);
        if !matches!(current.status, JobStatus::Completed | JobStatus::Cancelled)
            || crate::log_retention::TerminalLogManifest::from_job(current) != recovered_manifest
        {
            return Ok(());
        }
        let _ = crate::log_retention::write_terminal_manifest(&self.state_dir, current);
        Ok(())
    }

    /// Atomically request cancellation. Running jobs retain their status and
    /// claims until their exact owner acknowledges process-tree death.
    pub fn request_cancel(
        &mut self,
        job_id: &str,
        reason: Option<String>,
    ) -> QueueResult<Option<Job>> {
        self.request_cancel_with_proof(job_id, reason, None)
    }

    pub(crate) fn request_cancel_with_proof(
        &mut self,
        job_id: &str,
        reason: Option<String>,
        proof: Option<CancellationProof>,
    ) -> QueueResult<Option<Job>> {
        self.with_jobs_locked(|jobs| {
            let Some(job) = jobs.iter_mut().find(|job| job.id == job_id) else {
                return Ok(None);
            };
            let requested = job
                .request_cancel_with_reason_and_proof(reason, proof)
                .map_err(|error| QueueError::StateConflict(error.to_string()))?;
            *job = requested.clone();
            let _ = trim_terminal(jobs);
            Ok(Some(requested))
        })
    }

    /// Look up a job by id.
    pub fn get(&mut self, job_id: &str) -> QueueResult<Option<Job>> {
        let jobs = self.read_jobs_locked()?;
        Ok(jobs.iter().find(|job| job.id == job_id).cloned())
    }

    /// Return the currently running job, if any.
    pub fn get_active(&mut self) -> QueueResult<Option<Job>> {
        Ok(self.get_running()?.into_iter().next())
    }

    /// Return running jobs in queue storage order.
    pub fn get_running(&mut self) -> QueueResult<Vec<Job>> {
        let jobs = self.read_jobs_locked()?;
        Ok(jobs
            .into_iter()
            .filter(|job| job.status == JobStatus::Running)
            .collect())
    }

    /// Return completed or cancelled jobs newest first.
    pub fn get_recent(&mut self, limit: usize) -> QueueResult<Vec<Job>> {
        let jobs = self.read_jobs_locked()?;
        let mut completed = jobs
            .into_iter()
            .filter(|job| is_terminal_job(job.status))
            .collect::<Vec<_>>();
        sort_recent_completed(&mut completed);
        completed.truncate(limit);
        Ok(completed)
    }

    /// Return pending jobs sorted by priority descending, then FIFO.
    pub fn get_pending(&mut self) -> QueueResult<Vec<Job>> {
        let jobs = self.read_jobs_locked()?;
        Ok(pending_jobs_sorted(&jobs))
    }

    /// Return all durable jobs in queue storage order.
    pub fn get_all(&mut self) -> QueueResult<Vec<Job>> {
        self.read_jobs_locked()
    }

    /// Return all durable jobs without treating malformed queue state as empty.
    ///
    /// Ordinary queue inspection preserves the historical tolerant read used
    /// for startup and status display. Recovery decisions must instead fail
    /// closed: an unreadable live owner is not proof that work is absent.
    pub(crate) fn get_all_strict(&mut self) -> QueueResult<Vec<Job>> {
        let _lock = StateLock::acquire(self.state_lock_file())?;
        self.read_jobs_from_disk_strict()
    }

    /// Count pending jobs.
    pub fn pending_count(&mut self) -> QueueResult<usize> {
        let jobs = self.read_jobs_locked()?;
        Ok(jobs
            .iter()
            .filter(|job| job.status == JobStatus::Pending)
            .count())
    }

    /// Count running jobs.
    pub fn running_count(&mut self) -> QueueResult<usize> {
        let jobs = self.read_jobs_locked()?;
        Ok(jobs
            .iter()
            .filter(|job| job.status == JobStatus::Running)
            .count())
    }

    /// Try to acquire exclusive drain ownership.
    pub fn acquire_drain_lock(&self) -> QueueResult<Option<DrainLock>> {
        DrainLock::acquire(&self.lock_file()).map_err(QueueError::Io)
    }

    /// Recover stale running jobs. The caller must hold the drain lock.
    pub fn recover_stale_running_jobs_for_drain(
        &mut self,
        _drain_lock: &DrainLock,
    ) -> QueueResult<Vec<Job>> {
        self.with_jobs_locked(|jobs| {
            let recovered = recover_stale_running_jobs(jobs);
            let _ = trim_terminal(jobs);
            Ok(recovered)
        })
    }

    /// Recover only selected running jobs. Foreground drain owners use this to
    /// avoid terminalizing jobs whose durable envelopes assign ownership to a
    /// live daemon worker.
    pub fn recover_selected_running_jobs_for_drain(
        &mut self,
        _drain_lock: &DrainLock,
        job_ids: &[String],
    ) -> QueueResult<Vec<Job>> {
        let selected = job_ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
        self.with_jobs_locked(|jobs| {
            let recovered =
                recover_running_jobs_matching(jobs, |job| selected.contains(job.id.as_str()));
            let _ = trim_terminal(jobs);
            Ok(recovered)
        })
    }

    /// Trim old terminal jobs from durable queue state and return ids removed
    /// from `queue.json`. The caller must hold the drain lock.
    pub fn trim_terminal_jobs_for_drain(
        &mut self,
        _drain_lock: &DrainLock,
    ) -> QueueResult<Vec<String>> {
        self.with_jobs_locked(|jobs| Ok(trim_terminal(jobs)))
    }

    /// Cancel pending jobs whose durable request envelope is missing or unreadable.
    ///
    /// The caller must hold the drain lock and provide a request-envelope probe.
    pub fn cancel_orphan_pending_jobs_for_drain(
        &mut self,
        _drain_lock: &DrainLock,
        mut request_status: impl FnMut(&Job) -> Result<bool, String>,
    ) -> QueueResult<Vec<Job>> {
        self.with_jobs_locked(|jobs| {
            let mut cancelled_jobs = Vec::new();
            for job in jobs.iter_mut() {
                if job.status != JobStatus::Pending {
                    continue;
                }

                let reason = match request_status(job) {
                    Ok(true) => continue,
                    Ok(false) => ORPHAN_REQUEST_MESSAGE.to_owned(),
                    Err(error) => format!("{ORPHAN_REQUEST_MESSAGE}: {error}"),
                };

                if let Ok(cancelled) = job.cancel_with_reason(Some(reason)) {
                    *job = cancelled.clone();
                    cancelled_jobs.push(cancelled);
                }
            }
            let _ = trim_terminal(jobs);
            Ok(cancelled_jobs)
        })
    }

    /// Transition selected pending jobs to running. The caller must hold the
    /// drain lock.
    pub fn start_pending_jobs_for_drain(
        &mut self,
        _drain_lock: &DrainLock,
        job_ids: &[String],
    ) -> QueueResult<Vec<Job>> {
        self.with_jobs_locked(|jobs| {
            let mut started_jobs = Vec::new();
            let mut seen = BTreeSet::new();
            for job_id in job_ids {
                if !seen.insert(job_id.as_str()) {
                    continue;
                }
                let Some(job) = jobs
                    .iter_mut()
                    .find(|job| job.id == *job_id && job.status == JobStatus::Pending)
                else {
                    continue;
                };
                if let Ok(started) = job.start() {
                    *job = started.clone();
                    started_jobs.push(started);
                }
            }
            Ok(started_jobs)
        })
    }

    /// Cancel selected pending jobs by id. The caller must hold the drain lock.
    pub fn cancel_pending_jobs_for_drain(
        &mut self,
        _drain_lock: &DrainLock,
        cancellations: &[QueuePendingCancellation],
    ) -> QueueResult<Vec<Job>> {
        self.with_jobs_locked(|jobs| {
            let mut cancelled_jobs = Vec::new();
            let mut seen = BTreeSet::new();
            for cancellation in cancellations {
                if !seen.insert(cancellation.job_id.as_str()) {
                    continue;
                }
                let Some(job) = jobs
                    .iter_mut()
                    .find(|job| job.id == cancellation.job_id && job.status == JobStatus::Pending)
                else {
                    continue;
                };
                if let Ok(cancelled) = job.cancel_with_reason_and_proof(
                    Some(cancellation.reason.clone()),
                    cancellation.proof.clone(),
                ) {
                    *job = cancelled.clone();
                    cancelled_jobs.push(cancelled);
                }
            }
            let _ = trim_terminal(jobs);
            Ok(cancelled_jobs)
        })
    }

    /// Cancel the given jobs only if, re-checked under the state lock, they are
    /// still `Running` and still stale by heartbeat age. Returns the jobs that
    /// were actually cancelled (terminal `Cancelled`, with `reason`).
    ///
    /// Unlike [`recover_stale_running_jobs_for_drain`], this is safe to call
    /// while workers may be alive — the under-lock staleness re-check means a
    /// job that produced a fresh heartbeat after the caller's snapshot is never
    /// cancelled, so a live worker is not reaped out from under itself. It does
    /// not require the drain lock: it is a recovery action keyed on per-job
    /// liveness, not a drain-owned scheduling decision.
    ///
    /// [`recover_stale_running_jobs_for_drain`]: Self::recover_stale_running_jobs_for_drain
    pub fn cancel_stale_running_jobs(
        &mut self,
        job_ids: &[String],
        now: DateTime<Utc>,
        stale_after: chrono::Duration,
        reason: &str,
    ) -> QueueResult<Vec<Job>> {
        self.with_jobs_locked(|jobs| {
            let mut cancelled_jobs = Vec::new();
            let mut seen = BTreeSet::new();
            for job_id in job_ids {
                if !seen.insert(job_id.as_str()) {
                    continue;
                }
                let Some(job) = jobs
                    .iter_mut()
                    .find(|job| job.id == *job_id && job.is_stale_running(now, stale_after))
                else {
                    continue;
                };
                if let Ok(cancelled) = job.cancel_with_reason(Some(reason.to_owned())) {
                    *job = cancelled.clone();
                    cancelled_jobs.push(cancelled);
                }
            }
            let _ = trim_terminal(jobs);
            Ok(cancelled_jobs)
        })
    }

    /// Return selected running jobs to pending after scheduler-owned transient
    /// deferrals. The caller must hold the drain lock.
    pub fn requeue_deferred_running_jobs_for_drain(
        &mut self,
        _drain_lock: &DrainLock,
        requeues: &[QueueDeferredRequeue],
    ) -> QueueResult<Vec<Job>> {
        self.requeue_deferred_running_jobs(requeues)
    }

    /// Return one exactly fenced daemon worker to pending after a transient
    /// scheduler deferral. The caller must first verify the worker receipt;
    /// unlike a drain owner, the worker owns only its own job id.
    pub(crate) fn requeue_deferred_daemon_worker(
        &mut self,
        requeue: QueueDeferredRequeue,
    ) -> QueueResult<Option<Job>> {
        self.with_jobs_locked(|jobs| {
            let Some(job) = jobs
                .iter_mut()
                .find(|job| job.id == requeue.job_id && job.status == JobStatus::Running)
            else {
                return Ok(None);
            };
            if job.cancel_requested_at.is_none() {
                job.scheduler_defer_reason = Some(requeue.reason);
                job.scheduler_defer_count = job.scheduler_defer_count.saturating_add(1);
                job.scheduler_defer_until = requeue.defer_until;
            }
            Ok(Some(job.clone()))
        })
    }

    /// Release a daemon-deferred Running claim only after its supervisor has
    /// verified that the exact worker tree is dead.
    pub(crate) fn finalize_deferred_daemon_worker(
        &mut self,
        job_id: &str,
    ) -> QueueResult<Option<Job>> {
        self.with_jobs_locked(|jobs| {
            let Some(job) = jobs.iter_mut().find(|job| {
                job.id == job_id
                    && job.status == JobStatus::Running
                    && job.cancel_requested_at.is_none()
                    && job.scheduler_defer_reason.is_some()
            }) else {
                return Ok(None);
            };
            job.status = JobStatus::Pending;
            Ok(Some(job.clone()))
        })
    }

    fn requeue_deferred_running_jobs(
        &mut self,
        requeues: &[QueueDeferredRequeue],
    ) -> QueueResult<Vec<Job>> {
        self.with_jobs_locked(|jobs| {
            let mut requeued_jobs = Vec::new();
            let mut seen = BTreeSet::new();
            for requeue in requeues {
                if !seen.insert(requeue.job_id.as_str()) {
                    continue;
                }
                let Some(job) = jobs
                    .iter_mut()
                    .find(|job| job.id == requeue.job_id && job.status == JobStatus::Running)
                else {
                    continue;
                };
                if job.cancel_requested_at.is_some() {
                    requeued_jobs.push(job.clone());
                    continue;
                }
                if let Ok(deferred) =
                    job.defer_for_scheduler(requeue.reason.clone(), requeue.defer_until)
                {
                    *job = deferred.clone();
                    requeued_jobs.push(deferred);
                }
            }
            Ok(requeued_jobs)
        })
    }

    /// Complete one running job with an explicit fail-closed uncertainty
    /// result. This is used only after durable worker ownership is proven lost;
    /// arbitrary validation commands are never replayed automatically.
    pub fn complete_running_uncertain(
        &mut self,
        job_id: &str,
        reason: &str,
    ) -> QueueResult<Option<Job>> {
        self.with_jobs_locked(|jobs| {
            let Some(job) = jobs
                .iter_mut()
                .find(|job| job.id == job_id && job.status == JobStatus::Running)
            else {
                return Ok(None);
            };
            if job.cancel_requested_at.is_some() {
                return Ok(Some(job.clone()));
            }
            for target_name in &job.target_names {
                let mut result = stale_recovery_result(target_name);
                result.error_message = Some(reason.to_owned());
                result.failure_class = Some("UNCERTAIN".to_owned());
                job.results.insert(target_name.clone(), result);
            }
            job.status = JobStatus::Completed;
            job.completed_at = Some(Utc::now());
            let completed = job.clone();
            crate::log_retention::invalidate_conflicting_terminal_manifest(
                &self.state_dir,
                &completed,
            )?;
            let _ = trim_terminal(jobs);
            Ok(Some(completed))
        })
    }

    /// Exact-CAS terminalization for a separately audited receiptless orphan.
    ///
    /// This is intentionally narrower than ordinary cancellation: callers
    /// must first prove daemon/process absence and authenticated disposition,
    /// then supply the unchanged running snapshot observed under that audit.
    /// No other queue row is trimmed, reordered, or otherwise mutated.
    pub(crate) fn finalize_audited_receiptless_cancel(
        &mut self,
        expected_jobs: &[Job],
        expected: &Job,
        reason: String,
        proof: CancellationProof,
    ) -> QueueResult<Option<Job>> {
        self.with_jobs_locked_strict(|jobs| {
            if jobs.as_slice() != expected_jobs {
                return Err(QueueError::StateConflict(
                    "queue changed after receiptless orphan audit".to_owned(),
                ));
            }
            let Some(job) = jobs.iter_mut().find(|job| job.id == expected.id) else {
                return Ok(None);
            };
            if *job != *expected {
                return Err(QueueError::StateConflict(format!(
                    "receiptless orphan {} changed after audit",
                    expected.id
                )));
            }
            if job.status != JobStatus::Running || job.cancel_requested_at.is_none() {
                return Err(QueueError::StateConflict(format!(
                    "receiptless orphan {} is not a cancel-requested running job",
                    expected.id
                )));
            }
            let cancelled = job
                .cancel_with_reason_and_proof(Some(reason), Some(proof))
                .map_err(|error| QueueError::StateConflict(error.to_string()))?;
            *job = cancelled.clone();
            crate::log_retention::invalidate_conflicting_terminal_manifest(
                &self.state_dir,
                &cancelled,
            )?;
            Ok(Some(cancelled))
        })
    }

    /// Reclassify the exact completed snapshot produced by an authoritative
    /// worker when its required post-validation phase fails. The full snapshot
    /// comparison prevents a stale worker from rewriting a newer disposition.
    pub fn reclassify_completed_uncertain(
        &mut self,
        expected: &Job,
        reason: &str,
    ) -> QueueResult<Option<Job>> {
        self.with_jobs_locked(|jobs| {
            let Some(job) = jobs
                .iter_mut()
                .find(|job| job.id == expected.id && **job == *expected)
            else {
                return Ok(None);
            };
            if job.status != JobStatus::Completed {
                return Ok(None);
            }
            for target_name in &job.target_names {
                let mut result = stale_recovery_result(target_name);
                result.error_message = Some(reason.to_owned());
                result.failure_class = Some("UNCERTAIN".to_owned());
                job.results.insert(target_name.clone(), result);
            }
            let completed = job.clone();
            crate::log_retention::invalidate_conflicting_terminal_manifest(
                &self.state_dir,
                &completed,
            )?;
            let _ = trim_terminal(jobs);
            Ok(Some(completed))
        })
    }

    /// Mutate the queue under the short-lived state lock.
    pub fn with_jobs_locked<T>(
        &self,
        f: impl FnOnce(&mut Vec<Job>) -> QueueResult<T>,
    ) -> QueueResult<T> {
        let _lock = StateLock::acquire(self.state_lock_file())?;
        let mut jobs = self.read_jobs_from_disk()?;
        let output = f(&mut jobs)?;
        self.save_jobs_to_disk(&jobs)?;
        Ok(output)
    }

    /// Mutate recovery-owned queue state only after a strict read under the
    /// same lock. This prevents a malformed payload that appears after the
    /// recovery selection pass from being replaced as if it were empty.
    fn with_jobs_locked_strict<T>(
        &self,
        f: impl FnOnce(&mut Vec<Job>) -> QueueResult<T>,
    ) -> QueueResult<T> {
        let _lock = StateLock::acquire(self.state_lock_file())?;
        let mut jobs = self.read_jobs_from_disk_strict()?;
        let output = f(&mut jobs)?;
        self.save_jobs_to_disk(&jobs)?;
        Ok(output)
    }

    fn read_jobs_locked(&self) -> QueueResult<Vec<Job>> {
        let _lock = StateLock::acquire(self.state_lock_file())?;
        self.read_jobs_from_disk()
    }

    fn read_jobs_from_disk(&self) -> QueueResult<Vec<Job>> {
        let queue_file = self.queue_file();
        let raw = match fs::read_to_string(&queue_file) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        Ok(parse_jobs_payload(&raw))
    }

    fn read_jobs_from_disk_strict(&self) -> QueueResult<Vec<Job>> {
        let queue_file = self.queue_file();
        let raw = match fs::read_to_string(&queue_file) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        parse_jobs_payload_strict(&raw)
    }

    fn save_jobs_to_disk(&self, jobs: &[Job]) -> QueueResult<()> {
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(&self.state_dir)?;
        fs::create_dir_all(&self.state_dir)?;
        self.sweep_legacy_tmp();

        let payload = json!({
            "jobs": jobs.iter().map(Job::to_json_value).collect::<Vec<_>>(),
        });
        let payload = format!("{}\n", serde_json::to_string_pretty(&payload)?);
        let (temp_path, mut temp_file) = create_unique_temp_file(&self.state_dir)?;

        let result = (|| -> QueueResult<()> {
            temp_file.write_all(payload.as_bytes())?;
            temp_file.flush()?;
            temp_file.sync_all()?;
            drop(temp_file);
            replace_file_with_windows_retry(&temp_path, &self.queue_file())?;
            sync_directory_best_effort(&self.state_dir);
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }

    fn sweep_legacy_tmp(&self) {
        let mut legacy_tmp = self.queue_file();
        legacy_tmp.set_extension("json.tmp");
        let _ = fs::remove_file(legacy_tmp);
    }
}

fn backfill_pending_workload_scopes(jobs: &mut [Job], request_store: Option<&QueueRequestStore>) {
    let Some(request_store) = request_store else {
        return;
    };
    for job in jobs
        .iter_mut()
        .filter(|job| job.status == JobStatus::Pending && job.workload_scope.is_none())
    {
        let Ok(Some(envelope)) = request_store.load(&job.id) else {
            continue;
        };
        if recovery_envelope_matches_job(&envelope, job) {
            job.workload_scope = Some(envelope.workload_scope());
        }
    }
}

fn recovery_envelope_matches_job(envelope: &QueuedExecutionEnvelope, job: &Job) -> bool {
    if envelope.job_id != job.id {
        return false;
    }
    let request_targets = match &envelope.request {
        QueuedExecutionRequest::Run(request) => &request.targets,
        QueuedExecutionRequest::Ship(request) => &request.targets,
    };
    if envelope.resource_plan.targets != job.target_names
        || request_targets
            .iter()
            .map(|target| target.name.as_str())
            .ne(job.target_names.iter().map(String::as_str))
    {
        return false;
    }
    // Priority is queue policy, not workload identity: `queue bump` updates the
    // queued job without rewriting its immutable execution envelope.
    match (&envelope.kind, &envelope.request) {
        (QueuedExecutionKind::Run, QueuedExecutionRequest::Run(request)) => {
            matches!(job.kind, None | Some(JobKind::Run))
                && request.sha == job.sha
                && request.branch == job.branch
                && request.mode == job.mode
        }
        (QueuedExecutionKind::Ship, QueuedExecutionRequest::Ship(request)) => {
            matches!(job.kind, None | Some(JobKind::Ship))
                && request.sha == job.sha
                && request.branch == job.branch
                && request.mode == job.mode
        }
        _ => false,
    }
}

fn backfill_recovery_owner_scopes(jobs: &mut [Job], request_store: Option<&QueueRequestStore>) {
    let Some(request_store) = request_store else {
        return;
    };
    for job in jobs.iter_mut().filter(|job| job.workload_scope.is_none()) {
        let Ok(Some(envelope)) = request_store.load(&job.id) else {
            continue;
        };
        if recovery_envelope_matches_job(&envelope, job) {
            job.workload_scope = Some(envelope.workload_scope());
        }
    }
}

/// Opaque queue-state guard used to serialize cleanup with terminal publication.
pub(crate) struct QueueStateGuard {
    _inner: StateLock,
}

fn cancel_superseded_pending(jobs: &mut [Job], job: &Job) {
    for queued in jobs.iter_mut().filter(|queued| {
        same_workload_scope(queued, job)
            && queued.branch == job.branch
            && queued.status == JobStatus::Pending
            && queued.target_names == job.target_names
            && queued.mode == job.mode
    }) {
        if let Ok(cancelled) = queued.cancel_with_reason(Some(SUPERSEDED_MESSAGE.to_owned())) {
            *queued = cancelled;
        }
    }
}

fn same_workload_scope(left: &Job, right: &Job) -> bool {
    match (&left.workload_scope, &right.workload_scope) {
        (Some(left), Some(right)) => left == right,
        // Preserve the legacy queue contract for old callers and persisted
        // jobs that predate workload scopes. A scoped job never supersedes an
        // unscoped one because their ownership cannot be proven identical.
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

fn recover_stale_running_jobs(jobs: &mut [Job]) -> Vec<Job> {
    recover_running_jobs_matching(jobs, |_| true)
}

fn recover_running_jobs_matching(
    jobs: &mut [Job],
    mut selected: impl FnMut(&Job) -> bool,
) -> Vec<Job> {
    let mut recovered = Vec::new();
    for job in jobs
        .iter_mut()
        .filter(|job| job.status == JobStatus::Running && selected(job))
    {
        for target_name in &job.target_names {
            job.results
                .entry(target_name.clone())
                .or_insert_with(|| stale_recovery_result(target_name));
        }
        job.status = JobStatus::Completed;
        job.completed_at = Some(Utc::now());
        recovered.push(job.clone());
    }
    recovered
}

fn pending_jobs_sorted(jobs: &[Job]) -> Vec<Job> {
    let mut pending = jobs
        .iter()
        .filter(|job| job.status == JobStatus::Pending)
        .cloned()
        .collect::<Vec<_>>();
    pending.sort_by(compare_pending_jobs);
    pending
}

fn trim_terminal(jobs: &mut Vec<Job>) -> Vec<String> {
    let before_terminal = jobs
        .iter()
        .filter(|job| is_terminal_job(job.status))
        .map(|job| job.id.clone())
        .collect::<BTreeSet<_>>();
    let mut completed = jobs
        .iter()
        .filter(|job| is_terminal_job(job.status))
        .cloned()
        .collect::<Vec<_>>();
    sort_recent_completed(&mut completed);
    completed.truncate(KEEP_COMPLETED);

    let mut retained = jobs
        .iter()
        .filter(|job| !is_terminal_job(job.status))
        .cloned()
        .collect::<Vec<_>>();
    let retained_terminal = completed
        .iter()
        .map(|job| job.id.clone())
        .collect::<BTreeSet<_>>();
    retained.extend(completed);
    *jobs = retained;
    before_terminal
        .difference(&retained_terminal)
        .cloned()
        .collect()
}

fn is_terminal_job(status: JobStatus) -> bool {
    matches!(status, JobStatus::Completed | JobStatus::Cancelled)
}

/// Short-lived queue state mutation lock.
#[derive(Debug)]
struct StateLock {
    file: File,
}

/// Short-lived, workload-scoped ownership admission fence.
#[derive(Debug)]
pub(crate) struct WorkloadAdmissionLock {
    _state: StateLock,
}

impl StateLock {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        let writer_domain = crate::writer_domain_lease::acquire_for_protected_creation(&path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        drop(writer_domain);
        file.lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Exclusive queue-drain ownership guard.
#[derive(Debug)]
pub struct DrainLock {
    file: Option<File>,
}

impl DrainLock {
    fn acquire(path: &Path) -> io::Result<Option<Self>> {
        let writer_domain = crate::writer_domain_lease::acquire_for_protected_creation(path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        drop(writer_domain);
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
                file.set_len(0)?;
                writeln!(file, "{}", process::id())?;
                file.sync_all()?;
                Ok(Some(Self { file: Some(file) }))
            }
            Err(error) if lock_is_contended(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Release the lock early.
    pub fn release(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.take() {
            file.unlock()?;
        }
        Ok(())
    }
}

fn lock_is_contended(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }

    #[cfg(windows)]
    {
        // Windows reports byte-range lock contention as ERROR_LOCK_VIOLATION.
        error.raw_os_error() == Some(33)
    }

    #[cfg(not(windows))]
    {
        false
    }
}

impl Drop for DrainLock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// Retry policy for replacing the durable queue file on Windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplaceRetryPolicy {
    /// Maximum replace attempts.
    pub attempts: usize,
    /// Linear backoff unit.
    pub base_delay: Duration,
}

impl Default for ReplaceRetryPolicy {
    fn default() -> Self {
        Self {
            attempts: WINDOWS_REPLACE_ATTEMPTS,
            base_delay: WINDOWS_REPLACE_BASE_DELAY,
        }
    }
}

/// Atomically replace `dst` with `src`, using jittered retry on Windows.
///
/// POSIX gets a single rename attempt. Windows retries `PermissionDenied`
/// because `MoveFileEx` can transiently fail when a peer writer is
/// mid-rename or the destination is briefly open.
pub fn replace_file_with_windows_retry(src: &Path, dst: &Path) -> io::Result<()> {
    retry_replace_with_strategy(
        cfg!(windows),
        ReplaceRetryPolicy::default(),
        || fs::rename(src, dst),
        thread::sleep,
        random_jitter,
    )
}

/// Testable retry loop used by `replace_file_with_windows_retry`.
pub fn retry_replace_with_strategy<R, S, J>(
    is_windows: bool,
    policy: ReplaceRetryPolicy,
    mut replace: R,
    mut sleep: S,
    mut jitter: J,
) -> io::Result<()>
where
    R: FnMut() -> io::Result<()>,
    S: FnMut(Duration),
    J: FnMut(Duration) -> Duration,
{
    if !is_windows {
        return replace();
    }

    let attempts = policy.attempts.max(1);
    let mut last_error = None;

    for attempt_index in 0..attempts {
        match replace() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                last_error = Some(error);
                if attempt_index + 1 == attempts {
                    break;
                }
                let base = scaled_delay(policy.base_delay, attempt_index + 1);
                sleep((base / 2) + jitter(base));
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_error.expect("permission error recorded before retry exhaustion"))
}

fn scaled_delay(base: Duration, multiplier: usize) -> Duration {
    let nanos = base.as_nanos().saturating_mul(multiplier as u128);
    let capped = nanos.min(u128::from(u64::MAX));
    Duration::from_nanos(u64::try_from(capped).unwrap_or(u64::MAX))
}

fn random_jitter(max: Duration) -> Duration {
    let max_nanos = max.as_nanos();
    if max_nanos == 0 {
        return Duration::ZERO;
    }
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    Duration::from_nanos(u64::try_from(seed % max_nanos).unwrap_or(u64::MAX))
}

fn parse_jobs_payload(raw: &str) -> Vec<Job> {
    if raw.trim().is_empty() {
        return Vec::new();
    }

    let parsed: Value = match serde_json::from_str(raw) {
        Ok(parsed) => parsed,
        Err(_) => return Vec::new(),
    };
    let Some(jobs) = parsed.get("jobs").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut parsed_jobs = Vec::with_capacity(jobs.len());
    for job in jobs {
        let Ok(parsed_job) = serde_json::from_value::<Job>(job.clone()) else {
            return Vec::new();
        };
        parsed_jobs.push(parsed_job);
    }
    parsed_jobs
}

fn parse_jobs_payload_strict(raw: &str) -> QueueResult<Vec<Job>> {
    let parsed: Value = serde_json::from_str(raw)?;
    let jobs = parsed.get("jobs").ok_or_else(|| {
        QueueError::StateConflict("durable queue payload is missing its jobs array".to_owned())
    })?;
    serde_json::from_value(jobs.clone()).map_err(QueueError::Json)
}

fn compare_pending_jobs(left: &Job, right: &Job) -> Ordering {
    right
        .priority
        .value()
        .cmp(&left.priority.value())
        .then_with(|| left.created_at.cmp(&right.created_at))
}

fn sort_recent_completed(jobs: &mut [Job]) {
    jobs.sort_by(|left, right| completed_sort_time(right).cmp(completed_sort_time(left)));
}

fn completed_sort_time(job: &Job) -> &DateTime<Utc> {
    job.completed_at.as_ref().unwrap_or(&job.created_at)
}

fn stale_recovery_result(target_name: &str) -> TargetResult {
    let mut result = TargetResult::new(
        target_name.to_owned(),
        "unknown",
        TargetStatus::Error,
        "unknown",
    );
    result.error_message = Some(STALE_RECOVERY_MESSAGE.to_owned());
    result
}

fn create_unique_temp_file(state_dir: &Path) -> io::Result<(PathBuf, File)> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for attempt in 0..100 {
        let path = state_dir.join(format!(
            ".queue-{}-{stamp}-{attempt}.json.tmp",
            process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create unique queue temp file",
    ))
}

fn sync_directory_best_effort(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::time::Duration;

    use chrono::Utc;
    use serde_json::Value;
    use tempfile::TempDir;

    use crate::job::{
        CancellationCause, CancellationProof, Job, JobStatus, Priority, TargetResult, TargetStatus,
        ValidationMode,
    };
    use crate::queue_request::{QueueRequestStore, QueuedExecutionEnvelope, run_workload_scope};
    use crate::ship::RunExecutionRequest;

    use super::{
        KEEP_COMPLETED, ORPHAN_REQUEST_MESSAGE, Queue, QueueDeferredRequeue,
        QueuePendingCancellation, RecoveryEnqueue, ReplaceRetryPolicy, STALE_RECOVERY_MESSAGE,
        STALE_RUNNING_CANCEL_REASON, SUPERSEDED_MESSAGE, WINDOWS_REPLACE_ATTEMPTS,
        retry_replace_with_strategy, scaled_delay,
    };

    fn queue_dir() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn job(branch: &str, sha: &str, targets: &[&str]) -> Job {
        Job::create(
            sha,
            branch,
            targets.iter().map(|target| (*target).to_owned()).collect(),
            ValidationMode::Full,
            Priority::Normal,
        )
    }

    fn completed_from(mut job: Job, seconds_ago: i64) -> Job {
        job = job.start().expect("start").complete().expect("complete");
        job.completed_at = Some(Utc::now() - chrono::Duration::seconds(seconds_ago));
        job
    }

    fn running_aged(branch: &str, sha: &str, started_secs_ago: i64) -> Job {
        let mut running = job(branch, sha, &["mac"]).start().expect("start");
        running.started_at = Some(Utc::now() - chrono::Duration::seconds(started_secs_ago));
        running
    }

    #[test]
    fn cancel_stale_running_jobs_cancels_only_stale_running() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let stale = running_aged("main", "stale", 1000);
        let fresh = running_aged("main", "fresh", 5);
        let stale_id = stale.id.clone();
        let fresh_id = fresh.id.clone();
        queue.enqueue(stale).expect("stale");
        queue.enqueue(fresh).expect("fresh");

        let cancelled = queue
            .cancel_stale_running_jobs(
                &[stale_id.clone(), fresh_id.clone()],
                Utc::now(),
                chrono::Duration::seconds(180),
                STALE_RUNNING_CANCEL_REASON,
            )
            .expect("cancel");

        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].id, stale_id);
        let stale_job = queue.get(&stale_id).expect("get").expect("job");
        assert_eq!(stale_job.status, JobStatus::Cancelled);
        assert_eq!(
            stale_job.cancellation_reason.as_deref(),
            Some(STALE_RUNNING_CANCEL_REASON)
        );
        // The fresh running job is left untouched — no live worker is reaped.
        let fresh_job = queue.get(&fresh_id).expect("get").expect("job");
        assert_eq!(fresh_job.status, JobStatus::Running);
    }

    #[test]
    fn cancel_stale_running_jobs_ignores_non_running_and_dedupes() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        // Distinct branch/targets so enqueue supersession never touches it.
        let pending = job("feat-other", "pending", &["linux"]);
        let stale = running_aged("main", "stale", 1000);
        let pending_id = pending.id.clone();
        let stale_id = stale.id.clone();
        queue.enqueue(pending).expect("pending");
        queue.enqueue(stale).expect("stale");

        let cancelled = queue
            .cancel_stale_running_jobs(
                &[pending_id.clone(), stale_id.clone(), stale_id.clone()],
                Utc::now(),
                chrono::Duration::seconds(180),
                STALE_RUNNING_CANCEL_REASON,
            )
            .expect("cancel");

        // Pending is never cancelled by this path; the stale running job is
        // cancelled exactly once despite the duplicate id.
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].id, stale_id);
        assert_eq!(
            queue.get(&pending_id).expect("get").expect("job").status,
            JobStatus::Pending
        );
    }

    #[test]
    fn cancellation_request_wins_stale_completion_and_keeps_claims_until_ack() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let pending = queue
            .enqueue(job("main", "sha", &["mac"]))
            .expect("enqueue");
        let running = pending.start().expect("start");
        queue.update(&running).expect("running");
        let requested = queue
            .request_cancel(&running.id, Some("operator cancel".to_owned()))
            .expect("request")
            .expect("job");
        assert_eq!(requested.status, JobStatus::Running);
        let stale_completion = running.complete().expect("stale completion");

        assert!(matches!(
            queue.update(&stale_completion),
            Err(super::QueueError::StateConflict(_))
        ));
        let stale_terminal_cancel = running
            .cancel_with_reason(Some("worker-side stale cancel".to_owned()))
            .expect("stale cancel");
        assert!(matches!(
            queue.update(&stale_terminal_cancel),
            Err(super::QueueError::StateConflict(_))
        ));
        assert_eq!(
            queue.get(&running.id).expect("read").expect("job").status,
            JobStatus::Running
        );
        let preserved = queue
            .complete_running_uncertain(&running.id, "must not become uncertain")
            .expect("preserve")
            .expect("job");
        assert_eq!(preserved.status, JobStatus::Running);
        assert_eq!(
            preserved.cancellation_reason.as_deref(),
            Some("operator cancel")
        );
    }

    #[test]
    fn audited_receiptless_cancel_is_exact_cas_and_preserves_other_rows() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let running = queue
            .enqueue(job("main", "merged-head", &["mac"]))
            .expect("enqueue")
            .start()
            .expect("start")
            .request_cancel_with_reason(Some("operator request".to_owned()))
            .expect("request cancel");
        queue.update(&running).expect("running");
        let pending = queue
            .enqueue(job("other", "pending-head", &["mac"]))
            .expect("pending");
        let pending_before = queue.get(&pending.id).expect("read").expect("pending");
        let exact_queue = queue.get_all().expect("exact queue");
        let proof = CancellationProof {
            cause: CancellationCause::AlreadyMerged,
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            head_sha: running.sha.clone(),
        };

        let cancelled = queue
            .finalize_audited_receiptless_cancel(
                &exact_queue,
                &running,
                super::ALREADY_MERGED_CANCEL_REASON.to_owned(),
                proof.clone(),
            )
            .expect("finalize")
            .expect("job");
        assert_eq!(cancelled.status, JobStatus::Cancelled);
        assert_eq!(cancelled.cancellation_proof, Some(proof));
        assert_eq!(
            queue.get(&pending.id).expect("read").expect("pending"),
            pending_before
        );

        assert!(matches!(
            queue.finalize_audited_receiptless_cancel(
                &exact_queue,
                &running,
                super::ALREADY_MERGED_CANCEL_REASON.to_owned(),
                cancelled.cancellation_proof.clone().expect("proof"),
            ),
            Err(super::QueueError::StateConflict(_))
        ));
    }

    #[test]
    fn uncertain_completion_overwrites_stale_passing_results() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let pending = queue
            .enqueue(job("main", "sha", &["mac"]))
            .expect("enqueue");
        let mut running = pending.start().expect("start");
        let mut passed = TargetResult::new("mac", "macos", TargetStatus::Pass, "local");
        passed.completed_at = Some(Utc::now());
        running = running.with_result(passed);
        queue.update(&running).expect("running with stale pass");

        let uncertain = queue
            .complete_running_uncertain(&running.id, "worker ownership lost")
            .expect("complete uncertain")
            .expect("job");
        let result = uncertain.results.get("mac").expect("target result");
        assert_eq!(result.status, TargetStatus::Error);
        assert_eq!(result.failure_class.as_deref(), Some("UNCERTAIN"));
        assert_eq!(
            result.error_message.as_deref(),
            Some("worker ownership lost")
        );
    }

    fn read_queue_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path.join("queue.json")).expect("queue json"))
            .expect("valid json")
    }

    #[test]
    fn enqueue_and_retrieve_job() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let job = job("main", "abc", &["mac"]);
        let id = job.id.clone();

        queue.enqueue(job).expect("enqueue");

        assert_eq!(queue.pending_count().expect("pending"), 1);
        assert_eq!(queue.get(&id).expect("get").expect("job").sha, "abc");
    }

    #[test]
    fn next_pending_prefers_priority_then_fifo() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let low = job("feat/low", "a", &["mac"]).with_priority(Priority::Low);
        let first_high = job("feat/high-a", "b", &["mac"]).with_priority(Priority::High);
        let second_high = job("feat/high-b", "c", &["mac"]).with_priority(Priority::High);
        let first_high_id = first_high.id.clone();

        queue.enqueue(low).expect("low");
        queue.enqueue(first_high).expect("first high");
        queue.enqueue(second_high).expect("second high");

        assert_eq!(
            queue.next_pending().expect("next").expect("job").id,
            first_high_id
        );
    }

    #[test]
    fn supersedence_replaces_pending_same_scope_only() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let old = job("feat/x", "old", &["mac"]).with_workload_scope("repo:pulp");
        let running = job("feat/x", "running", &["mac"])
            .with_workload_scope("repo:pulp")
            .start()
            .expect("start");
        let narrow = job("feat/x", "narrow", &["linux"]).with_workload_scope("repo:pulp");
        let smoke = Job::create(
            "smoke",
            "feat/x",
            vec!["mac".to_owned()],
            ValidationMode::Smoke,
            Priority::Normal,
        )
        .with_workload_scope("repo:pulp");
        let independent = job("feat/x", "independent", &["mac"]).with_workload_scope("repo:forge");
        let new = job("feat/x", "new", &["mac"]).with_workload_scope("repo:pulp");

        queue.enqueue(old).expect("old");
        queue.enqueue(running.clone()).expect("running");
        queue.update(&running).expect("update running");
        queue.enqueue(narrow).expect("narrow");
        queue.enqueue(smoke).expect("smoke");
        queue.enqueue(independent).expect("independent");
        queue.enqueue(new).expect("new");

        let pending = queue.get_pending().expect("pending");
        assert_eq!(queue.running_count().expect("running"), 1);
        assert_eq!(pending.len(), 4);
        assert!(pending.iter().any(|job| job.sha == "new"));
        assert!(pending.iter().any(|job| job.sha == "independent"));
        assert!(pending.iter().any(|job| job.sha == "narrow"));
        assert!(pending.iter().any(|job| job.sha == "smoke"));
        assert!(!pending.iter().any(|job| job.sha == "old"));
        let recent = queue.get_recent(5).expect("recent");
        let superseded = recent
            .iter()
            .find(|job| job.sha == "old")
            .expect("superseded job retained");
        assert_eq!(superseded.status, JobStatus::Cancelled);
        assert_eq!(
            superseded.cancellation_reason.as_deref(),
            Some(SUPERSEDED_MESSAGE)
        );
    }

    #[test]
    fn recovery_enqueue_ignores_older_terminal_owner_but_fences_newer_terminal_owner() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let mut older = completed_from(
            job("feat/x", "old", &["mac"]).with_workload_scope("repo:pulp"),
            60,
        );
        older.created_at = Utc::now() - chrono::Duration::minutes(2);
        let recovery = job("feat/x", "recovery", &["mac"]).with_workload_scope("repo:pulp");
        queue.enqueue(older).expect("older terminal");
        assert_eq!(
            queue
                .enqueue_recovery_if_unowned(recovery.clone(), recovery.created_at)
                .expect("recovery enqueue"),
            RecoveryEnqueue::Inserted
        );

        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let mut recovery = job("feat/x", "recovery", &["mac"]).with_workload_scope("repo:pulp");
        recovery.created_at = Utc::now() - chrono::Duration::minutes(2);
        let newer = completed_from(
            job("feat/x", "newer", &["mac"]).with_workload_scope("repo:pulp"),
            0,
        );
        let newer_id = newer.id.clone();
        queue.enqueue(newer).expect("newer terminal");
        assert_eq!(
            queue
                .enqueue_recovery_if_unowned(recovery.clone(), recovery.created_at)
                .expect("recovery enqueue"),
            RecoveryEnqueue::OwnedBy(newer_id)
        );
    }

    #[test]
    fn recovery_enqueue_fails_closed_on_unknown_active_unscoped_owner() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let unknown = job("unknown", "unknown", &["mac"]);
        let unknown_id = unknown.id.clone();
        let mismatched_request = RunExecutionRequest {
            branch: "unrelated".to_owned(),
            sha: "different".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: Vec::new(),
        };
        QueueRequestStore::new(temp.path())
            .expect("request store")
            .save(&QueuedExecutionEnvelope::from_run_request(
                unknown_id.clone(),
                temp.path(),
                &mismatched_request,
            ))
            .expect("mismatched envelope");
        queue.enqueue(unknown).expect("unknown owner");
        let recovery = job("feat/x", "recovery", &["mac"]).with_workload_scope("repo:pulp");
        assert_eq!(
            queue
                .enqueue_recovery_if_unowned(recovery.clone(), recovery.created_at)
                .expect("recovery enqueue"),
            RecoveryEnqueue::OwnedBy(unknown_id)
        );
    }

    #[test]
    fn enqueue_backfills_legacy_pending_run_scope_from_its_durable_envelope() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let legacy = job("feat/x", "old", &[]);
        let legacy_id = legacy.id.clone();
        let request = RunExecutionRequest {
            branch: "feat/x".to_owned(),
            sha: "old".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: Vec::new(),
        };
        QueueRequestStore::new(temp.path())
            .expect("request store")
            .save(&QueuedExecutionEnvelope::from_run_request(
                legacy_id.clone(),
                temp.path(),
                &request,
            ))
            .expect("legacy envelope");
        queue.enqueue(legacy).expect("legacy");

        let replacement =
            job("feat/x", "new", &[]).with_workload_scope(run_workload_scope(temp.path()));
        queue.enqueue(replacement).expect("replacement");

        let legacy = queue.get(&legacy_id).expect("get").expect("legacy job");
        assert_eq!(legacy.status, JobStatus::Cancelled);
        assert_eq!(
            legacy.cancellation_reason.as_deref(),
            Some(SUPERSEDED_MESSAGE)
        );
    }

    #[test]
    fn enqueue_backfills_legacy_scope_after_queue_priority_bump() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let legacy = job("feat/x", "old", &[]);
        let legacy_id = legacy.id.clone();
        queue.enqueue(legacy.clone()).expect("legacy");

        let request = RunExecutionRequest {
            branch: "feat/x".to_owned(),
            sha: "old".to_owned(),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            targets: Vec::new(),
        };
        QueueRequestStore::new(temp.path())
            .expect("request store")
            .save(&QueuedExecutionEnvelope::from_run_request(
                legacy_id.clone(),
                temp.path(),
                &request,
            ))
            .expect("legacy envelope");

        queue
            .update(&legacy.with_priority(Priority::High))
            .expect("priority bump");
        let replacement =
            job("feat/x", "new", &[]).with_workload_scope(run_workload_scope(temp.path()));
        queue.enqueue(replacement).expect("replacement");

        let legacy = queue.get(&legacy_id).expect("get").expect("legacy job");
        assert_eq!(legacy.status, JobStatus::Cancelled);
        assert_eq!(
            legacy.cancellation_reason.as_deref(),
            Some(SUPERSEDED_MESSAGE)
        );
    }

    #[test]
    fn update_get_active_and_persistence_round_trip() {
        let temp = queue_dir();
        let state_dir = temp.path().to_path_buf();
        let mut queue = Queue::new(&state_dir).expect("queue");
        let job = job("main", "abc", &["mac"]);
        let id = job.id.clone();
        queue.enqueue(job.clone()).expect("enqueue");
        assert!(queue.get_active().expect("active").is_none());

        let started = job.start().expect("start");
        queue.update(&started).expect("update");
        assert_eq!(
            queue.get_active().expect("active").expect("job").status,
            JobStatus::Running
        );

        let mut reopened = Queue::new(&state_dir).expect("reopen");
        assert_eq!(
            reopened.get(&id).expect("get").expect("job").status,
            JobStatus::Running
        );
    }

    #[test]
    fn update_rejects_missing_job_id() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        let missing = job("main", "abc", &["mac"]);

        assert!(matches!(
            queue.update(&missing),
            Err(super::QueueError::StateConflict(reason)) if reason.contains("not present")
        ));
    }

    #[test]
    fn held_drain_lock_prevents_stale_running_recovery() {
        let temp = queue_dir();
        let state_dir = temp.path().to_path_buf();
        let mut queue = Queue::new(&state_dir).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("held");
        let started = job("main", "abc", &["mac"]).start().expect("start");
        let id = started.id.clone();
        queue.enqueue(started).expect("enqueue");
        drop(queue);

        let mut reopened = Queue::new(&state_dir).expect("reopen");
        assert_eq!(
            reopened.get(&id).expect("get").expect("job").status,
            JobStatus::Running
        );
        drop(lock);
    }

    #[test]
    fn non_drain_open_does_not_recover_or_mutate_running_jobs() {
        let temp = queue_dir();
        let state_dir = temp.path().to_path_buf();
        let mut queue = Queue::new(&state_dir).expect("queue");
        let started = job("main", "abc", &["mac", "linux"])
            .start()
            .expect("start")
            .with_result(TargetResult::new(
                "mac",
                "macos",
                TargetStatus::Pass,
                "local",
            ));
        let id = started.id.clone();
        queue.enqueue(started).expect("enqueue");
        drop(queue);
        let before = fs::read_to_string(state_dir.join("queue.json")).expect("before");

        let mut reopened = Queue::new(&state_dir).expect("reopen");
        let still_running = reopened.get(&id).expect("get").expect("job");
        let after = fs::read_to_string(state_dir.join("queue.json")).expect("after");

        assert_eq!(still_running.status, JobStatus::Running);
        assert_eq!(before, after);
    }

    #[test]
    fn drain_owner_recovers_stale_running_jobs() {
        let temp = queue_dir();
        let state_dir = temp.path().to_path_buf();
        let mut queue = Queue::new(&state_dir).expect("queue");
        let started = job("main", "abc", &["mac", "linux"])
            .start()
            .expect("start")
            .with_result(TargetResult::new(
                "mac",
                "macos",
                TargetStatus::Pass,
                "local",
            ));
        let id = started.id.clone();
        queue.enqueue(started).expect("enqueue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("held");

        queue
            .recover_stale_running_jobs_for_drain(&lock)
            .expect("recover");
        let recovered = queue.get(&id).expect("get").expect("job");

        assert_eq!(recovered.status, JobStatus::Completed);
        assert_eq!(recovered.results["mac"].status, TargetStatus::Pass);
        assert_eq!(recovered.results["linux"].status, TargetStatus::Error);
        assert_eq!(
            recovered.results["linux"].error_message.as_deref(),
            Some(STALE_RECOVERY_MESSAGE)
        );
    }

    #[test]
    fn drain_owner_cancels_orphan_pending_jobs() {
        let temp = queue_dir();
        let state_dir = temp.path().to_path_buf();
        let mut queue = Queue::new(&state_dir).expect("queue");
        let present = job("main", "present", &["mac"]);
        let missing = job("main", "missing", &["linux"]);
        let unreadable = job("main", "unreadable", &["windows"]);
        let running = job("running", "running", &["mac"]).start().expect("start");
        let completed = job("done", "complete", &["linux"])
            .start()
            .expect("start")
            .complete()
            .expect("complete");
        let present_id = present.id.clone();
        let missing_id = missing.id.clone();
        let unreadable_id = unreadable.id.clone();
        let running_id = running.id.clone();
        let completed_id = completed.id.clone();

        queue.enqueue(present).expect("present");
        queue.enqueue(missing).expect("missing");
        queue.enqueue(unreadable).expect("unreadable");
        queue.enqueue(running).expect("running");
        queue.enqueue(completed).expect("completed");
        let lock = queue.acquire_drain_lock().expect("lock").expect("held");

        let cancelled = queue
            .cancel_orphan_pending_jobs_for_drain(&lock, |job| match job.sha.as_str() {
                "present" => Ok(true),
                "missing" => Ok(false),
                "unreadable" => Err("permission denied".to_owned()),
                other => panic!("unexpected probe for {other}"),
            })
            .expect("cancel orphans");

        assert_eq!(cancelled.len(), 2);
        assert_eq!(
            queue
                .get(&present_id)
                .expect("present")
                .expect("job")
                .status,
            JobStatus::Pending
        );
        assert_eq!(
            queue
                .get(&running_id)
                .expect("running")
                .expect("job")
                .status,
            JobStatus::Running
        );
        assert_eq!(
            queue
                .get(&completed_id)
                .expect("completed")
                .expect("job")
                .status,
            JobStatus::Completed
        );

        let missing = queue.get(&missing_id).expect("missing").expect("job");
        let unreadable = queue.get(&unreadable_id).expect("unreadable").expect("job");
        assert_eq!(missing.status, JobStatus::Cancelled);
        assert_eq!(
            missing.cancellation_reason.as_deref(),
            Some(ORPHAN_REQUEST_MESSAGE)
        );
        assert_eq!(unreadable.status, JobStatus::Cancelled);
        assert_eq!(
            unreadable.cancellation_reason.as_deref(),
            Some("Queued request envelope missing or unreadable: permission denied")
        );
        assert!(
            !queue
                .get_pending()
                .expect("pending")
                .iter()
                .any(|job| job.id == missing_id || job.id == unreadable_id)
        );
        let recent = queue.get_recent(10).expect("recent");
        assert!(recent.iter().any(|job| job.id == missing_id));
        assert!(recent.iter().any(|job| job.id == unreadable_id));
    }

    #[test]
    fn drain_owner_starts_selected_pending_jobs_in_requested_order() {
        let temp = queue_dir();
        let state_dir = temp.path().to_path_buf();
        let mut queue = Queue::new(&state_dir).expect("queue");
        let first = job("main", "first", &["mac"]);
        let second = job("main", "second", &["linux"]);
        let running = job("main", "running", &["windows"]).start().expect("start");
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        let running_id = running.id.clone();

        queue.enqueue(first).expect("first");
        queue.enqueue(second).expect("second");
        queue.enqueue(running).expect("running");
        let lock = queue.acquire_drain_lock().expect("lock").expect("held");

        let started = queue
            .start_pending_jobs_for_drain(
                &lock,
                &[
                    second_id.clone(),
                    first_id.clone(),
                    second_id.clone(),
                    running_id.clone(),
                ],
            )
            .expect("start selected");

        assert_eq!(
            started
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            [second_id.as_str(), first_id.as_str()]
        );
        assert!(started.iter().all(|job| job.status == JobStatus::Running));
        assert_eq!(
            queue.get(&first_id).expect("first").expect("job").status,
            JobStatus::Running
        );
        assert_eq!(
            queue.get(&second_id).expect("second").expect("job").status,
            JobStatus::Running
        );
        assert_eq!(
            queue
                .get(&running_id)
                .expect("running")
                .expect("job")
                .status,
            JobStatus::Running
        );
    }

    #[test]
    fn drain_owner_cancels_selected_pending_jobs_by_id() {
        let temp = queue_dir();
        let state_dir = temp.path().to_path_buf();
        let mut queue = Queue::new(&state_dir).expect("queue");
        let cancel = job("main", "cancel", &["mac"]);
        let keep = job("main", "keep", &["linux"]);
        let running = job("main", "running", &["windows"]).start().expect("start");
        let cancel_id = cancel.id.clone();
        let keep_id = keep.id.clone();
        let running_id = running.id.clone();

        queue.enqueue(cancel).expect("cancel");
        queue.enqueue(keep).expect("keep");
        queue.enqueue(running).expect("running");
        let lock = queue.acquire_drain_lock().expect("lock").expect("held");

        let cancelled = queue
            .cancel_pending_jobs_for_drain(
                &lock,
                &[
                    QueuePendingCancellation {
                        job_id: cancel_id.clone(),
                        reason: "same PR superseded".to_owned(),
                        proof: None,
                    },
                    QueuePendingCancellation {
                        job_id: cancel_id.clone(),
                        reason: "duplicate ignored".to_owned(),
                        proof: None,
                    },
                    QueuePendingCancellation {
                        job_id: running_id.clone(),
                        reason: "running ignored".to_owned(),
                        proof: None,
                    },
                ],
            )
            .expect("cancel selected");

        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].id, cancel_id);
        assert_eq!(cancelled[0].status, JobStatus::Cancelled);
        assert_eq!(
            cancelled[0].cancellation_reason.as_deref(),
            Some("same PR superseded")
        );
        assert_eq!(
            queue.get(&keep_id).expect("keep").expect("job").status,
            JobStatus::Pending
        );
        assert_eq!(
            queue
                .get(&running_id)
                .expect("running")
                .expect("job")
                .status,
            JobStatus::Running
        );
        let recent = queue.get_recent(10).expect("recent");
        assert!(recent.iter().any(|job| job.id == cancel_id));
    }

    #[test]
    fn drain_owner_requeues_scheduler_deferred_running_jobs() {
        let temp = queue_dir();
        let state_dir = temp.path().to_path_buf();
        let mut queue = Queue::new(&state_dir).expect("queue");
        let running = job("main", "running", &["mac", "linux"])
            .start()
            .expect("start")
            .with_result(TargetResult::new(
                "mac",
                "macos",
                TargetStatus::Running,
                "host-pool:local_macs/mac",
            ))
            .with_result(TargetResult::new(
                "linux",
                "linux",
                TargetStatus::Pass,
                "local",
            ));
        let pending = job("main", "pending", &["windows"]);
        let running_id = running.id.clone();
        let pending_id = pending.id.clone();
        let retry_at = Utc::now();

        queue.enqueue(running).expect("running");
        queue.enqueue(pending).expect("pending");
        let lock = queue.acquire_drain_lock().expect("lock").expect("held");

        let requeued = queue
            .requeue_deferred_running_jobs_for_drain(
                &lock,
                &[
                    QueueDeferredRequeue {
                        job_id: running_id.clone(),
                        reason: "host_pool_lease_unavailable".to_owned(),
                        defer_until: Some(retry_at),
                    },
                    QueueDeferredRequeue {
                        job_id: running_id.clone(),
                        reason: "duplicate ignored".to_owned(),
                        defer_until: None,
                    },
                    QueueDeferredRequeue {
                        job_id: pending_id.clone(),
                        reason: "pending ignored".to_owned(),
                        defer_until: None,
                    },
                ],
            )
            .expect("requeue selected");

        assert_eq!(requeued.len(), 1);
        assert_eq!(requeued[0].id, running_id);
        assert_eq!(requeued[0].status, JobStatus::Pending);
        assert_eq!(requeued[0].started_at, None);
        assert_eq!(
            requeued[0].scheduler_defer_reason.as_deref(),
            Some("host_pool_lease_unavailable")
        );
        assert_eq!(requeued[0].scheduler_defer_count, 1);
        assert_eq!(requeued[0].scheduler_defer_until, Some(retry_at));
        assert!(!requeued[0].results.contains_key("mac"));
        assert_eq!(
            requeued[0].results.get("linux").map(|result| result.status),
            Some(TargetStatus::Pass)
        );
        assert_eq!(
            queue
                .get(&pending_id)
                .expect("pending")
                .expect("job")
                .status,
            JobStatus::Pending
        );
    }

    #[test]
    fn two_queue_handles_preserve_independent_progress_updates() {
        let temp = queue_dir();
        let state_dir = temp.path().to_path_buf();
        let mut first = Queue::new(&state_dir).expect("first queue");
        let mut second = Queue::new(&state_dir).expect("second queue");
        let mac_job = job("main", "abc", &["mac"]);
        let linux_job = job("feature", "def", &["linux"]);
        let mac_id = mac_job.id.clone();
        let linux_id = linux_job.id.clone();

        first.enqueue(mac_job.clone()).expect("enqueue mac");
        first.enqueue(linux_job.clone()).expect("enqueue linux");
        let mac_started = mac_job.start().expect("start mac");
        let linux_started = linux_job.start().expect("start linux");
        first.update(&mac_started).expect("start mac update");
        second.update(&linux_started).expect("start linux update");

        let mac = mac_started.with_result(TargetResult::new(
            "mac",
            "macos",
            TargetStatus::Pass,
            "local",
        ));
        first.update(&mac).expect("mac update");

        let linux = linux_started.with_result(TargetResult::new(
            "linux",
            "linux",
            TargetStatus::Pass,
            "ssh",
        ));
        second.update(&linux).expect("linux update");

        let final_mac = first.get(&mac_id).expect("get mac").expect("mac job");
        let final_linux = first.get(&linux_id).expect("get linux").expect("linux job");
        assert_eq!(final_mac.results["mac"].status, TargetStatus::Pass);
        assert_eq!(final_linux.results["linux"].status, TargetStatus::Pass);
    }

    #[test]
    fn recent_completed_jobs_are_newest_first_and_trimmed() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");

        for index in 0..(KEEP_COMPLETED + 10) {
            let pending = job(&format!("feat/{index}"), &format!("sha{index}"), &["mac"]);
            let completed = completed_from(
                pending.clone(),
                i64::try_from(KEEP_COMPLETED + 10 - index).expect("seconds"),
            );
            queue.enqueue(pending).expect("enqueue");
            queue.update(&completed).expect("update");
        }

        let recent = queue.get_recent(100).expect("recent");
        assert_eq!(recent.len(), KEEP_COMPLETED);
        assert_eq!(recent.first().expect("first").sha, "sha34");
    }

    #[test]
    fn drain_owned_terminal_trim_returns_removed_job_ids() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        for index in 0..(KEEP_COMPLETED + 2) {
            let pending = job(&format!("feat/{index}"), &format!("sha{index}"), &["mac"]);
            let completed = completed_from(
                pending.clone(),
                i64::try_from(KEEP_COMPLETED + 2 - index).expect("seconds"),
            );
            queue.enqueue(pending).expect("enqueue");
            queue.update(&completed).expect("update");
        }

        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("acquired");
        let removed = queue
            .trim_terminal_jobs_for_drain(&drain_lock)
            .expect("trim");

        assert!(removed.is_empty());

        let stale = completed_from(job("old", "sha-old", &["mac"]), 10_000);
        let stale_id = stale.id.clone();
        queue
            .with_jobs_locked(|jobs| {
                jobs.push(stale);
                Ok(())
            })
            .expect("inject stale");

        let removed = queue
            .trim_terminal_jobs_for_drain(&drain_lock)
            .expect("trim stale");
        assert_eq!(removed, vec![stale_id]);
        assert_eq!(queue.get_recent(100).expect("recent").len(), KEEP_COMPLETED);
    }

    #[test]
    fn empty_missing_zero_byte_and_corrupt_queue_files_load_as_empty() {
        for contents in [None, Some(""), Some("   "), Some(r#"{"jobs": [{"id":"#)] {
            let temp = queue_dir();
            if let Some(contents) = contents {
                fs::write(temp.path().join("queue.json"), contents).expect("write");
            }
            let mut queue = Queue::new(temp.path()).expect("queue");
            assert_eq!(queue.pending_count().expect("pending"), 0);
        }
    }

    #[test]
    fn strict_queue_read_distinguishes_missing_valid_and_corrupt_state() {
        let temp = queue_dir();
        let mut queue = Queue::new(temp.path()).expect("queue");
        assert!(queue.get_all_strict().expect("missing queue").is_empty());

        let expected = job("main", "abc", &["mac"]);
        queue.enqueue(expected.clone()).expect("enqueue");
        assert_eq!(queue.get_all_strict().expect("valid queue"), vec![expected]);

        fs::write(
            queue.queue_file(),
            r#"{"jobs":[{"id":"truncated-live-owner"}]}"#,
        )
        .expect("corrupt queue");
        assert!(matches!(
            queue.get_all_strict(),
            Err(super::QueueError::Json(_))
        ));
    }

    #[test]
    fn save_writes_atomic_json_sweeps_legacy_tmp_and_leaves_no_temp_files() {
        let temp = queue_dir();
        fs::write(temp.path().join("queue.json.tmp"), "legacy").expect("legacy");
        let mut queue = Queue::new(temp.path()).expect("queue");

        queue
            .enqueue(job("main", "abc", &["mac"]))
            .expect("enqueue");

        assert!(read_queue_json(temp.path())["jobs"].as_array().is_some());
        assert!(!temp.path().join("queue.json.tmp").exists());
        let leftovers = fs::read_dir(temp.path())
            .expect("read dir")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".queue-"))
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "orphan queue temp files: {leftovers:?}"
        );
    }

    #[test]
    fn drain_lock_is_exclusive_and_releases_on_drop_or_manual_release() {
        let temp = queue_dir();
        let queue = Queue::new(temp.path()).expect("queue");
        let mut first = queue.acquire_drain_lock().expect("first").expect("lock");

        assert!(queue.acquire_drain_lock().expect("second").is_none());
        first.release().expect("release");

        let second = queue.acquire_drain_lock().expect("third").expect("lock");
        drop(second);
        assert!(queue.acquire_drain_lock().expect("after drop").is_some());
    }

    #[test]
    fn posix_path_attempts_once_without_sleep_or_jitter() {
        let mut replace_calls = 0;
        let mut sleep_calls = 0;
        let mut jitter_calls = 0;

        retry_replace_with_strategy(
            false,
            ReplaceRetryPolicy::default(),
            || {
                replace_calls += 1;
                Ok(())
            },
            |_| sleep_calls += 1,
            |_| {
                jitter_calls += 1;
                Duration::ZERO
            },
        )
        .expect("replace");

        assert_eq!(replace_calls, 1);
        assert_eq!(sleep_calls, 0);
        assert_eq!(jitter_calls, 0);
    }

    #[test]
    fn windows_path_uses_centered_growing_jittered_backoff() {
        let attempts = Cell::new(0);
        let mut sleeps = Vec::new();
        let mut jitter_bounds = Vec::new();

        retry_replace_with_strategy(
            true,
            ReplaceRetryPolicy::default(),
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() <= 3 {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "busy"))
                } else {
                    Ok(())
                }
            },
            |duration| sleeps.push(duration),
            |bound| {
                jitter_bounds.push(bound);
                bound / 2
            },
        )
        .expect("eventual success");

        assert_eq!(attempts.get(), 4);
        assert_eq!(
            jitter_bounds,
            vec![
                Duration::from_millis(50),
                Duration::from_millis(100),
                Duration::from_millis(150),
            ]
        );
        assert_eq!(
            sleeps,
            vec![
                Duration::from_millis(50),
                Duration::from_millis(100),
                Duration::from_millis(150),
            ]
        );
    }

    #[test]
    fn windows_path_surfaces_permission_error_after_budget() {
        let mut replace_calls = 0;
        let error = retry_replace_with_strategy(
            true,
            ReplaceRetryPolicy::default(),
            || {
                replace_calls += 1;
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "access denied",
                ))
            },
            |_| {},
            |_| Duration::ZERO,
        )
        .expect_err("budget exhausted");

        assert_eq!(replace_calls, WINDOWS_REPLACE_ATTEMPTS);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "access denied");
    }

    #[test]
    fn windows_path_does_not_retry_non_permission_errors() {
        let mut replace_calls = 0;
        let error = retry_replace_with_strategy(
            true,
            ReplaceRetryPolicy::default(),
            || {
                replace_calls += 1;
                Err(io::Error::new(io::ErrorKind::NotFound, "missing tmp"))
            },
            |_| panic!("should not sleep"),
            |_| panic!("should not draw jitter"),
        )
        .expect_err("not found");

        assert_eq!(replace_calls, 1);
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn scaled_delay_saturates() {
        assert_eq!(
            scaled_delay(Duration::from_millis(50), 3),
            Duration::from_millis(150)
        );
        assert_eq!(
            scaled_delay(Duration::from_nanos(u64::MAX), 2),
            Duration::from_nanos(u64::MAX)
        );
    }
}
