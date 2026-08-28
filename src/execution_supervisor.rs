//! Durable daemon ownership for queued execution workers.
//!
//! The supervisor deliberately never replays a job that reached `Running`.
//! A verified live worker may be adopted after daemon restart; otherwise a
//! stale running job becomes an explicit `UNCERTAIN` terminal outcome.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io;
#[cfg(unix)]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

use crate::execution_termination::{TerminationAction, TerminationPhase, TerminationStore};
use crate::host_pool::{HostPoolLeaseError, HostPoolLeaseStore, default_lease_path};
use crate::identity::RuntimeMode;
use crate::job::{
    CancellationCause, CancellationProof, DEFAULT_RUNNING_JOB_STALE_SECONDS, Job, JobStatus,
};
use crate::queue::{Queue, QueueDeferredRequeue, QueueError, QueuePendingCancellation};
use crate::queue_request::{
    QueueOutcomeStore, QueueRequestError, QueueRequestStore, QueuedExecutionEnvelope,
    QueuedExecutionKind, QueuedExecutionOutcome, QueuedExecutionOwner, QueuedExecutionRequest,
};
use crate::queue_scheduler::{AlreadyMergedCancellation, AlreadyMergedObserver};
use crate::ship::persist_terminal_outcome;

const MAX_WORKERS: usize = 1;
const MAX_METADATA_CONTROLLERS: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkerObservation {
    Alive(WorkerReceipt),
    Dead,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessLiveness {
    #[cfg_attr(windows, allow(dead_code))]
    Alive,
    Dead,
    Unknown,
}

/// One durable worker identity used for restart adoption and PID-reuse defense.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerReceipt {
    /// Queue job id.
    pub job_id: String,
    /// Unpredictable generation passed on the worker command line.
    pub generation: String,
    /// Worker process id.
    pub pid: u32,
    /// Worker launch timestamp.
    pub started_at: chrono::DateTime<Utc>,
}

/// Cross-process ownership transaction for worker receipt publication,
/// replacement, validation, and deletion.
pub(crate) struct WorkerReceiptOwnershipGuard(File);

impl Drop for WorkerReceiptOwnershipGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

pub(crate) fn acquire_worker_receipt_ownership_lock(
    state_dir: &Path,
) -> io::Result<WorkerReceiptOwnershipGuard> {
    let worker_dir = state_dir.join("queue-workers");
    crate::writer_domain_lease::ensure_protected_dir_all(&worker_dir)?;
    let lock_path = worker_dir.join(".ownership.lock");
    let writer_domain = crate::writer_domain_lease::acquire_for_protected_creation(&lock_path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    drop(writer_domain);
    FileExt::lock_exclusive(&file)?;
    Ok(WorkerReceiptOwnershipGuard(file))
}

/// Errors surfaced by one supervisor tick.
#[derive(Debug)]
pub enum SupervisorError {
    /// Queue state failed.
    Queue(QueueError),
    /// Request-envelope state failed.
    Request(QueueRequestError),
    /// Worker process or receipt I/O failed.
    Io(io::Error),
    /// Host-pool lease state failed.
    HostPool(HostPoolLeaseError),
    /// Terminal-outcome persistence failed.
    Outcome(String),
}

impl std::fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queue(error) => write!(formatter, "{error}"),
            Self::Request(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "execution supervisor I/O failed: {error}"),
            Self::HostPool(error) => write!(formatter, "execution supervisor {error}"),
            Self::Outcome(error) => write!(formatter, "execution outcome failed: {error}"),
        }
    }
}

impl std::error::Error for SupervisorError {}
impl From<QueueError> for SupervisorError {
    fn from(value: QueueError) -> Self {
        Self::Queue(value)
    }
}
impl From<QueueRequestError> for SupervisorError {
    fn from(value: QueueRequestError) -> Self {
        Self::Request(value)
    }
}
impl From<io::Error> for SupervisorError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<HostPoolLeaseError> for SupervisorError {
    fn from(value: HostPoolLeaseError) -> Self {
        Self::HostPool(value)
    }
}

/// Same-host daemon supervisor. Dropping it never signals workers.
pub struct ExecutionSupervisor {
    binary: PathBuf,
    mode: RuntimeMode,
    global_dir: PathBuf,
    state_dir: PathBuf,
    children: BTreeMap<String, Child>,
    merge_observers: BTreeMap<PathBuf, (AlreadyMergedObserver, String)>,
    next_queue_absent_recovery: std::time::Instant,
    queue_absent_recovery_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct QueueAbsentRecoveryFlight(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for QueueAbsentRecoveryFlight {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl ExecutionSupervisor {
    /// Construct a supervisor for one durable state root.
    #[must_use]
    pub fn new(
        binary: PathBuf,
        mode: RuntimeMode,
        global_dir: PathBuf,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            binary,
            mode,
            global_dir,
            state_dir,
            children: BTreeMap::new(),
            merge_observers: BTreeMap::new(),
            next_queue_absent_recovery: std::time::Instant::now(),
            queue_absent_recovery_in_flight: std::sync::Arc::new(
                std::sync::atomic::AtomicBool::new(false),
            ),
        }
    }

    /// Reconcile worker ownership and admit safe pending jobs.
    pub fn tick(&mut self) -> Result<(), SupervisorError> {
        crate::writer_domain_lease::ensure_protected_dir_all(&self.worker_dir())?;
        self.observe_merged_ship_jobs()?;
        self.reconcile_terminal_outcomes()?;
        self.reconcile_finalized_termination_transactions()?;
        self.terminate_cancelled_workers()?;
        self.terminate_deferred_workers()?;
        self.reconcile_finalized_termination_transactions()?;
        // Drop exited children from the in-memory ownership map before queue-
        // absence recovery checks receipts. Otherwise a stale child-map entry
        // preserves its dead receipt during the recovery preflight and can
        // turn already-dead ownership into a durable needs-agent fence.
        self.reap_owned_children()?;
        self.recover_queue_absent()?;
        self.sweep_terminal_receipts()?;
        let unknown_worker = self.reconcile_running()?;
        if !unknown_worker {
            self.admit_pending()?;
        }
        Ok(())
    }

    fn recover_queue_absent(&mut self) -> Result<(), SupervisorError> {
        let now = std::time::Instant::now();
        if now < self.next_queue_absent_recovery {
            return Ok(());
        }
        if self
            .queue_absent_recovery_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }

        // Recovery treats any receipt for the ship as unresolved ownership.
        // Remove receipts whose exact process generation is already proven
        // dead before the detached sweep can turn that transient evidence into
        // a durable needs-agent fence. Live, malformed, and otherwise unknown
        // receipts remain in place and continue to block replay fail-closed.
        self.sweep_terminal_receipts()?;

        self.next_queue_absent_recovery = now + StdDuration::from_mins(1);
        if self
            .queue_absent_recovery_in_flight
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Ok(());
        }
        let in_flight = std::sync::Arc::clone(&self.queue_absent_recovery_in_flight);
        let state_dir = self.state_dir.clone();
        let global_dir = self.global_dir.clone();
        let mode = self.mode;
        thread::spawn(move || {
            let _flight = QueueAbsentRecoveryFlight(in_flight);
            if let Ok(config) =
                crate::config::LoadedConfig::load_machine_global_from_dir(global_dir.clone())
            {
                let _ = crate::queue_absent_recovery::recover_queue_absent_ships(
                    &state_dir,
                    mode,
                    &global_dir,
                    &config,
                );
            }
        });
        Ok(())
    }

    fn observe_merged_ship_jobs(&mut self) -> Result<(), SupervisorError> {
        let request_store = QueueRequestStore::new(&self.state_dir)?;
        let mut queue = Queue::new(&self.state_dir)?;
        let mut jobs_by_cwd =
            BTreeMap::<PathBuf, Vec<(Job, crate::queue_request::ExecutionProvenance)>>::new();
        for job in queue
            .get_all()?
            .into_iter()
            .filter(requires_merged_ship_observation)
        {
            let Ok(Some(envelope)) = request_store.load(&job.id) else {
                continue;
            };
            if envelope.job_id != job.id || !envelope.is_daemon_admissible() {
                continue;
            }
            let Some(provenance) = envelope.provenance.as_ref() else {
                continue;
            };
            if provenance.config_signature.is_none()
                || envelope.cwd != provenance.canonical_cwd
                || provenance.validate(&provenance.canonical_cwd).is_err()
                || !matches!(envelope.request, QueuedExecutionRequest::Ship(_))
            {
                continue;
            }
            jobs_by_cwd
                .entry(provenance.canonical_cwd.clone())
                .or_default()
                .push((job, provenance.clone()));
        }
        if jobs_by_cwd.is_empty() {
            self.merge_observers.clear();
            return Ok(());
        }

        let active_cwds = jobs_by_cwd.keys().cloned().collect::<BTreeSet<_>>();
        self.merge_observers
            .retain(|cwd, _| active_cwds.contains(cwd));
        let mut pending = Vec::new();
        let mut running = Vec::new();
        for (cwd, scoped_entries) in jobs_by_cwd {
            let Ok(config) = crate::config::LoadedConfig::load_from_cwd_with_global_dir(
                self.mode,
                &cwd,
                self.global_dir.clone(),
            ) else {
                continue;
            };
            let Some(signature) = scoped_entries.iter().find_map(|(_, provenance)| {
                provenance
                    .validate_with_config(&cwd, &config)
                    .ok()
                    .and_then(|()| provenance.config_signature.clone())
            }) else {
                continue;
            };
            if self
                .merge_observers
                .get(&cwd)
                .is_none_or(|(_, cached_signature)| cached_signature != &signature)
            {
                self.merge_observers.insert(
                    cwd.clone(),
                    (AlreadyMergedObserver::from_config(&config), signature),
                );
            }
            let (observer, trusted_signature) = self
                .merge_observers
                .get_mut(&cwd)
                .expect("observer inserted for active cwd");
            let scoped_jobs = scoped_entries
                .into_iter()
                .filter(|(_, provenance)| {
                    provenance.config_signature.as_ref() == Some(trusted_signature)
                })
                .map(|(job, _)| job)
                .collect::<Vec<_>>();
            pending.extend(observer.observe_pending(&scoped_jobs, &request_store, &cwd, None));
            running.extend(observer.observe_running(&scoped_jobs, &request_store, &cwd, None));
        }
        self.apply_merge_cancellations(pending, running)
    }

    fn apply_merge_cancellations(
        &self,
        pending: Vec<AlreadyMergedCancellation>,
        running: Vec<AlreadyMergedCancellation>,
    ) -> Result<(), SupervisorError> {
        let mut queue = Queue::new(&self.state_dir)?;
        if let Some(lock) = queue.acquire_drain_lock()? {
            let pending = pending
                .into_iter()
                .map(|item| QueuePendingCancellation {
                    job_id: item.job_id,
                    reason: crate::queue::ALREADY_MERGED_CANCEL_REASON.to_owned(),
                    proof: Some(CancellationProof {
                        cause: CancellationCause::AlreadyMerged,
                        repository: item.repository,
                        pull_request: item.pr,
                        head_sha: item.head_sha,
                    }),
                })
                .collect::<Vec<_>>();
            let cancelled = queue.cancel_pending_jobs_for_drain(&lock, &pending)?;
            for job in &cancelled {
                let _ = persist_terminal_outcome(job, &self.state_dir);
            }
        }
        for item in running {
            let _ = queue.request_cancel_with_proof(
                &item.job_id,
                Some(crate::queue::ALREADY_MERGED_CANCEL_REASON.to_owned()),
                Some(CancellationProof {
                    cause: CancellationCause::AlreadyMerged,
                    repository: item.repository,
                    pull_request: item.pr,
                    head_sha: item.head_sha,
                }),
            )?;
        }
        Ok(())
    }

    /// Repair typed outcomes from authoritative terminal queue records. Queue
    /// terminalization wins first; a transient outcome write is retried by the
    /// next daemon tick instead of leaving wait/watch without a disposition.
    fn reconcile_terminal_outcomes(&self) -> Result<(), SupervisorError> {
        let mut queue = Queue::new(&self.state_dir)?;
        let request_store = QueueRequestStore::new(&self.state_dir)?;
        let outcome_store = QueueOutcomeStore::new(&self.state_dir)?;
        for job in queue.get_recent(usize::MAX)? {
            let envelope = match request_store.load(&job.id) {
                Ok(Some(envelope)) => envelope,
                Ok(None) => continue,
                Err(error) if request_error_is_job_local(&error) => continue,
                Err(error) => return Err(error.into()),
            };
            let outcome = outcome_store.load(&job.id)?;
            let incomplete_ship = matches!(
                (&envelope.kind, &outcome),
                (
                    QueuedExecutionKind::Ship,
                    Some(QueuedExecutionOutcome::Ship {
                        post_validation: None,
                        ..
                    })
                )
            );
            let repair_incomplete_ship = incomplete_ship
                && !self.children.contains_key(&job.id)
                && matches!(self.observe_receipt(&job.id)?, WorkerObservation::Dead);
            if outcome.is_none() || repair_incomplete_ship {
                persist_terminal_outcome(&job, &self.state_dir)
                    .map_err(|error| SupervisorError::Outcome(error.to_string()))?;
            }
        }
        Ok(())
    }

    fn reap_owned_children(&mut self) -> Result<(), SupervisorError> {
        let mut exited = Vec::new();
        for (job_id, child) in &mut self.children {
            if let Some(status) = child.try_wait()? {
                exited.push((job_id.clone(), status.success()));
            }
        }
        for (job_id, success) in exited {
            let mut queue = Queue::new(&self.state_dir)?;
            let job = queue.get(&job_id)?;
            if matches!(job.as_ref().map(|job| job.status), Some(JobStatus::Running)) {
                if job
                    .as_ref()
                    .is_some_and(|job| job.cancel_requested_at.is_some())
                {
                    // The root exited before the supervisor could freeze and
                    // snapshot its tree. Retain ownership and claims rather
                    // than asserting that potentially reparented descendants
                    // are dead.
                    continue;
                }
                let reason = if success {
                    "worker exited without committing a terminal outcome"
                } else {
                    "worker process exited unexpectedly; side-effect state is uncertain"
                };
                if let Some(completed) = queue.complete_running_uncertain(&job_id, reason)? {
                    persist_terminal_outcome(&completed, &self.state_dir)
                        .map_err(|error| SupervisorError::Outcome(error.to_string()))?;
                }
            }
            self.children.remove(&job_id);
            self.remove_receipt_if_present(&job_id)?;
        }
        Ok(())
    }

    fn reconcile_running(&mut self) -> Result<bool, SupervisorError> {
        self.reconcile_running_with_probe(process_liveness)
    }

    fn reconcile_running_with_probe(
        &mut self,
        probe: impl Fn(&WorkerReceipt) -> ProcessLiveness + Copy,
    ) -> Result<bool, SupervisorError> {
        let mut queue = Queue::new(&self.state_dir)?;
        let request_store = QueueRequestStore::new(&self.state_dir)?;
        let mut unknown_worker = false;
        for job in queue.get_running()? {
            if self.children.contains_key(&job.id) {
                continue;
            }
            match self.observe_receipt_with_probe(&job.id, probe)? {
                WorkerObservation::Alive(_) => continue,
                WorkerObservation::Unknown => {
                    unknown_worker = true;
                    continue;
                }
                WorkerObservation::Dead => {}
            }
            // Only daemon-owned work belongs to this supervisor's recovery
            // boundary. Foreground or unreadable ownership must be preserved;
            // terminalizing it could race a live explicit --foreground drain.
            let daemon_owned = matches!(request_store.load(&job.id), Ok(Some(envelope))
            if envelope.job_id == job.id
                && envelope.is_daemon_admissible()
                && job.is_stale_running(
                    Utc::now(),
                    Duration::seconds(DEFAULT_RUNNING_JOB_STALE_SECONDS),
                ));
            if !daemon_owned {
                continue;
            }
            if job.cancel_requested_at.is_some() {
                // A missing receipt cannot prove that a worker tree never
                // started or has exited. Preserve the Running claim until an
                // exact live receipt can be stopped and verified.
                unknown_worker = true;
                continue;
            }
            if let Some(completed) = queue.complete_running_uncertain(
                &job.id,
                "durable worker ownership was lost; automatic replay is forbidden",
            )? {
                persist_terminal_outcome(&completed, &self.state_dir)
                    .map_err(|error| SupervisorError::Outcome(error.to_string()))?;
                self.remove_receipt_if_present(&job.id)?;
            }
        }
        Ok(unknown_worker)
    }

    fn terminate_cancelled_workers(&mut self) -> Result<(), SupervisorError> {
        let mut queue = Queue::new(&self.state_dir)?;
        let jobs = queue.get_all()?;
        let requested = jobs
            .iter()
            .filter(|job| job.status == JobStatus::Running && job.cancel_requested_at.is_some())
            .map(|job| job.id.clone())
            .collect::<BTreeSet<_>>();
        for job_id in &requested {
            if let Some(transaction) =
                self.complete_worker_termination(job_id, TerminationAction::Cancel)?
            {
                self.acknowledge_cancelled_job(&mut queue, job_id)?;
                self.cleanup_termination_transaction(&transaction)?;
            }
        }

        // Backward compatibility for jobs terminalized by an older cancel
        // command before this supervisor could confirm process-tree death.
        let cancelled = jobs
            .iter()
            .filter(|job| job.status == JobStatus::Cancelled)
            .map(|job| job.id.clone())
            .collect::<BTreeSet<_>>();
        for job_id in &cancelled {
            if let Some(transaction) =
                self.complete_worker_termination(job_id, TerminationAction::Cancel)?
            {
                self.cleanup_termination_transaction(&transaction)?;
            }
        }
        Ok(())
    }

    fn terminate_deferred_workers(&mut self) -> Result<(), SupervisorError> {
        self.terminate_deferred_workers_with_cleanup_hook(|_, _| {})
    }

    fn terminate_deferred_workers_with_cleanup_hook(
        &mut self,
        mut before_cleanup: impl FnMut(&str, &crate::execution_termination::TerminationTransaction),
    ) -> Result<(), SupervisorError> {
        let mut queue = Queue::new(&self.state_dir)?;
        let deferred = queue
            .get_running()?
            .into_iter()
            .filter(|job| job.cancel_requested_at.is_none() && job.scheduler_defer_reason.is_some())
            .map(|job| job.id)
            .collect::<Vec<_>>();
        for job_id in deferred {
            if let Some(transaction) =
                self.complete_worker_termination(&job_id, TerminationAction::Defer)?
            {
                let finalized = queue.finalize_deferred_daemon_worker(&job_id)?;
                if finalized.is_some() {
                    before_cleanup(&job_id, &transaction);
                    self.cleanup_termination_transaction(&transaction)?;
                } else if queue.get(&job_id)?.is_some_and(|job| {
                    job.status == JobStatus::Running && job.cancel_requested_at.is_some()
                }) {
                    self.acknowledge_cancelled_job(&mut queue, &job_id)?;
                    if queue
                        .get(&job_id)?
                        .is_none_or(|job| job.status != JobStatus::Running)
                    {
                        self.cleanup_termination_transaction(&transaction)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn reconcile_finalized_termination_transactions(&self) -> Result<(), SupervisorError> {
        let store = TerminationStore::new(&self.state_dir);
        let mut queue = Queue::new(&self.state_dir)?;
        for transaction in store.list()? {
            if transaction.phase != TerminationPhase::LeasesReleased {
                continue;
            }
            let Some(job) = queue.get(&transaction.job_id)? else {
                continue;
            };
            let finalized = match transaction.action {
                TerminationAction::Cancel => job.status == JobStatus::Cancelled,
                TerminationAction::Defer => {
                    job.status == JobStatus::Pending && job.scheduler_defer_reason.is_some()
                }
            };
            if finalized {
                self.cleanup_termination_transaction(&transaction)?;
            }
        }
        Ok(())
    }

    fn acknowledge_cancelled_job(
        &self,
        queue: &mut Queue,
        job_id: &str,
    ) -> Result<(), SupervisorError> {
        let Some(job) = queue.get(job_id)? else {
            return Ok(());
        };
        if job.status != JobStatus::Running || job.cancel_requested_at.is_none() {
            return Ok(());
        }
        let cancelled = job
            .cancel_with_reason_and_proof(
                job.cancellation_reason.clone(),
                job.cancellation_proof.clone(),
            )
            .map_err(|error| SupervisorError::Outcome(error.to_string()))?;
        queue.update(&cancelled)?;
        persist_terminal_outcome(&cancelled, &self.state_dir)
            .map_err(|error| SupervisorError::Outcome(error.to_string()))
    }

    fn sweep_terminal_receipts(&self) -> Result<(), SupervisorError> {
        // Queue-absence recovery validates orphan receipts as durable evidence
        // that the prior worker no longer owns this ship. The recovery runs in
        // a detached thread so GitHub I/O cannot stall the daemon; do not erase
        // that evidence from the same tick while the validation is in flight.
        // The next tick resumes bounded cleanup after the flight guard drops.
        if self
            .queue_absent_recovery_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        let mut queue = Queue::new(&self.state_dir)?;
        let running = queue
            .get_running()?
            .into_iter()
            .map(|job| job.id)
            .collect::<BTreeSet<_>>();
        for entry in fs::read_dir(self.worker_dir())? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(job_id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if running.contains(job_id) || self.children.contains_key(job_id) {
                continue;
            }
            let _ = self.observe_receipt(job_id)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // One scan owns cancellation, resource, and controller capacity decisions.
    fn admit_pending(&mut self) -> Result<(), SupervisorError> {
        let mut queue = Queue::new(&self.state_dir)?;
        let Some(lock) = queue.acquire_drain_lock()? else {
            return Ok(());
        };
        let request_store = QueueRequestStore::new(&self.state_dir)?;
        let running = queue.get_running()?;
        let running_resources = running_resource_claims(&running, &request_store)?;
        let mut occupied = running_resources.claims;
        let mut live_native_count = 0usize;
        let mut live_metadata_count = 0usize;
        for job in &running {
            match request_store.load(&job.id) {
                Ok(Some(envelope)) if envelope.is_metadata_authority_controller() => {
                    live_metadata_count += 1;
                }
                _ => live_native_count += 1,
            }
        }

        let pending = queue.get_pending()?;
        let mut selected = Vec::new();
        let mut selected_native_count = 0usize;
        let mut selected_metadata_count = 0usize;
        let mut cancellations = Vec::new();
        let now = Utc::now();
        for job in pending {
            if job
                .scheduler_defer_until
                .is_some_and(|defer_until| defer_until > now)
            {
                continue;
            }
            let envelope = match request_store.load(&job.id) {
                Ok(Some(envelope)) if envelope.job_id != job.id => {
                    cancellations.push(QueuePendingCancellation {
                        job_id: job.id,
                        reason: "queued execution request belongs to a different job; automatic execution is forbidden"
                            .to_owned(),
                        proof: None,
                    });
                    continue;
                }
                Ok(Some(envelope)) if envelope.is_daemon_admissible() => envelope,
                Ok(Some(envelope))
                    if envelope.execution_owner == QueuedExecutionOwner::Foreground =>
                {
                    // An explicit --foreground submitter owns this job. Do not
                    // race it for execution or convert it into a cancellation.
                    continue;
                }
                Ok(Some(_)) => {
                    cancellations.push(QueuePendingCancellation {
                        job_id: job.id,
                        reason: "legacy request lacks unattended-execution provenance; resubmit it or use --foreground"
                            .to_owned(),
                        proof: None,
                    });
                    continue;
                }
                Ok(None) => {
                    cancellations.push(QueuePendingCancellation {
                        job_id: job.id,
                        reason:
                            "queued execution request is missing; automatic execution is forbidden"
                                .to_owned(),
                        proof: None,
                    });
                    continue;
                }
                Err(error) if request_error_is_job_local(&error) => {
                    cancellations.push(QueuePendingCancellation {
                        job_id: job.id,
                        reason: format!(
                            "queued execution request is invalid; automatic execution is forbidden: {error}"
                        ),
                        proof: None,
                    });
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            // Keep scanning after worker capacity is full so malformed or
            // legacy pending envelopes cannot linger indefinitely behind a
            // valid job selected earlier in this tick.
            let metadata_controller = envelope.is_metadata_authority_controller();
            if metadata_controller {
                if live_metadata_count + selected_metadata_count >= MAX_METADATA_CONTROLLERS {
                    continue;
                }
            } else if live_native_count + selected_native_count >= MAX_WORKERS {
                continue;
            }
            if !admissible(&envelope, &occupied) {
                continue;
            }
            if metadata_controller {
                selected_metadata_count += 1;
            } else {
                selected_native_count += 1;
            }
            occupied.extend(resource_claims(&envelope));
            selected.push(job.id);
        }
        let cancelled = queue.cancel_pending_jobs_for_drain(&lock, &cancellations)?;
        for job in &cancelled {
            // A malformed envelope may not be convertible into a typed outcome;
            // the terminal queue record itself is still durable and must not
            // prevent unrelated work from advancing.
            let _ = persist_terminal_outcome(job, &self.state_dir);
        }
        // A running worker whose request cannot be loaded has unknown resource
        // ownership. Preserve pending work until that envelope is repaired or
        // the worker reaches a terminal state; treating the missing claims as
        // an empty set could double-book any host, VM, or repository it owns.
        let started = if running_resources.errors.is_empty() {
            queue.start_pending_jobs_for_drain(&lock, &selected)?
        } else {
            Vec::new()
        };
        for job in started {
            if let Err(error) = self.spawn_worker(&job) {
                queue.requeue_deferred_running_jobs_for_drain(
                    &lock,
                    &[QueueDeferredRequeue {
                        job_id: job.id.clone(),
                        reason: format!("worker spawn failed before execution: {error}"),
                        defer_until: Some(Utc::now() + Duration::seconds(5)),
                    }],
                )?;
            }
        }
        Ok(())
    }

    fn spawn_worker(&mut self, job: &Job) -> io::Result<()> {
        self.spawn_worker_with_receipt_writer(job, write_json_atomic)
    }

    fn spawn_worker_with_receipt_writer(
        &mut self,
        job: &Job,
        write_receipt: impl FnOnce(&Path, &WorkerReceipt) -> io::Result<()>,
    ) -> io::Result<()> {
        let generation = worker_generation()?;
        let retention_policy =
            crate::config::LoadedConfig::load_machine_global_from_dir(self.global_dir.clone())
                .map_or_else(
                    |_| crate::log_retention::LogRetentionPolicy::default(),
                    |config| crate::log_retention::LogRetentionPolicy::from_config(&config),
                );
        crate::log_retention::rotate_if_oversize(&self.log_path(&job.id), retention_policy)?;
        let log_path = self.log_path(&job.id);
        let writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&log_path)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        drop(writer_domain);
        let stderr = log.try_clone()?;
        let mut command = Command::new(&self.binary);
        command
            .arg("--mode")
            .arg(self.mode.as_str())
            .arg("--global-dir")
            .arg(&self.global_dir)
            .arg("--state-dir")
            .arg(&self.state_dir)
            .arg("execution-worker")
            .arg("--job-id")
            .arg(&job.id)
            .arg("--generation")
            .arg(&generation)
            .env(
                crate::writer_domain_lease::PROTECTED_STDIO_PATH_ENV,
                log_path,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let _receipt_lock = acquire_worker_receipt_ownership_lock(&self.state_dir)?;
        let mut child = command.spawn()?;
        let receipt = WorkerReceipt {
            job_id: job.id.clone(),
            generation,
            pid: child.id(),
            started_at: Utc::now(),
        };
        let receipt_path = self.receipt_path(&job.id);
        if let Err(error) = write_receipt(&receipt_path, &receipt) {
            let exact_receipt_is_visible = fs::read(&receipt_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<WorkerReceipt>(&bytes).ok())
                .is_some_and(|observed| observed == receipt);
            if exact_receipt_is_visible {
                // Rename committed but the durability fsync reported failure.
                // Retain the already-spawned child as the one owner rather than
                // requeueing a second execution.
                self.children.insert(job.id.clone(), child);
                return Ok(());
            }
            if terminate_child_tree(&mut child)? {
                remove_if_present(&receipt_path)?;
                return Err(error);
            }
            // Death could not be verified. Retain ownership and keep the queue
            // Running; returning an error here would requeue a second worker.
            self.children.insert(job.id.clone(), child);
            return Ok(());
        }
        self.children.insert(job.id.clone(), child);
        Ok(())
    }

    fn observe_receipt(&self, job_id: &str) -> io::Result<WorkerObservation> {
        self.observe_receipt_with_probe(job_id, process_liveness)
    }

    /// Observe an exact cancellation receipt without retiring dead evidence.
    /// Missing, malformed, and dead-root-only receipts remain unknown because
    /// none prove that potentially reparented descendants are dead. Only a
    /// live exact owner can be frozen, snapshotted, killed, and fully verified.
    fn observe_cancellation_receipt(&self, job_id: &str) -> io::Result<WorkerObservation> {
        let _receipt_lock = acquire_worker_receipt_ownership_lock(&self.state_dir)?;
        let path = self.receipt_path(job_id);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(WorkerObservation::Unknown);
            }
            Err(error) => return Err(error),
        };
        let Ok(receipt) = serde_json::from_slice::<WorkerReceipt>(&bytes) else {
            return Ok(WorkerObservation::Unknown);
        };
        if receipt.job_id != job_id {
            return Ok(WorkerObservation::Unknown);
        }
        Ok(match process_liveness(&receipt) {
            ProcessLiveness::Alive => WorkerObservation::Alive(receipt),
            ProcessLiveness::Dead | ProcessLiveness::Unknown => WorkerObservation::Unknown,
        })
    }

    fn read_exact_receipt(&self, job_id: &str) -> io::Result<Option<WorkerReceipt>> {
        let _receipt_lock = acquire_worker_receipt_ownership_lock(&self.state_dir)?;
        let bytes = match fs::read(self.receipt_path(job_id)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let receipt = serde_json::from_slice::<WorkerReceipt>(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if receipt.job_id != job_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker receipt job identity changed",
            ));
        }
        Ok(Some(receipt))
    }

    fn release_host_pool_leases(&self, job_id: &str) -> Result<(), SupervisorError> {
        HostPoolLeaseStore::new(default_lease_path(&self.state_dir)).release_for_job(job_id)?;
        Ok(())
    }

    fn cleanup_termination_transaction(
        &self,
        transaction: &crate::execution_termination::TerminationTransaction,
    ) -> Result<(), SupervisorError> {
        let _receipt_lock = acquire_worker_receipt_ownership_lock(&self.state_dir)?;
        let path = self.receipt_path(&transaction.job_id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                TerminationStore::new(&self.state_dir).remove(&transaction.job_id)?;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let observed = serde_json::from_slice::<WorkerReceipt>(&bytes).ok();
        if observed
            .as_ref()
            .is_some_and(|receipt| transaction.matches_receipt(receipt))
        {
            remove_if_present(&path)?;
        }
        TerminationStore::new(&self.state_dir).remove(&transaction.job_id)?;
        Ok(())
    }

    fn complete_worker_termination(
        &mut self,
        job_id: &str,
        action: TerminationAction,
    ) -> Result<Option<crate::execution_termination::TerminationTransaction>, SupervisorError> {
        let store = TerminationStore::new(&self.state_dir);
        let mut child = self.children.remove(job_id);
        let mut transaction = if let Some(transaction) = store.load(job_id)? {
            let mut transaction = transaction;
            // Once `begin` has durably recorded the frozen process-tree
            // snapshot, that transaction is the recovery authority. The
            // separately published worker receipt may disappear in a crash
            // after the transaction commit; requiring it here would strand a
            // tree that we can still prove dead from the durable snapshot.
            // A receipt that is present remains a generation CAS fence: never
            // apply an old transaction to a replacement worker generation.
            if let Some(receipt) = self.read_exact_receipt(job_id)?
                && !transaction.matches_receipt(&receipt)
            {
                if let Some(child) = child {
                    self.children.insert(job_id.to_owned(), child);
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "durable termination transaction receipt generation changed",
                )
                .into());
            }
            if transaction.action == TerminationAction::Defer && action == TerminationAction::Cancel
            {
                store.promote_to_cancel(&mut transaction)?;
            } else if transaction.action != action {
                if let Some(child) = child {
                    self.children.insert(job_id.to_owned(), child);
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "worker termination action changed",
                )
                .into());
            }
            transaction
        } else {
            let receipt = match self.observe_cancellation_receipt(job_id)? {
                WorkerObservation::Alive(receipt) => receipt,
                WorkerObservation::Dead | WorkerObservation::Unknown => {
                    if let Some(child) = child {
                        self.children.insert(job_id.to_owned(), child);
                    }
                    return Ok(None);
                }
            };
            store.begin(&receipt, action)?
        };
        if !store.prove_tree_dead(&mut transaction, child.as_mut())? {
            if let Some(child) = child {
                self.children.insert(job_id.to_owned(), child);
            }
            return Ok(None);
        }
        if transaction.phase < TerminationPhase::LeasesReleased {
            self.release_host_pool_leases(job_id)?;
            store.mark_leases_released(&mut transaction)?;
        }
        Ok(Some(transaction))
    }

    fn observe_receipt_with_probe(
        &self,
        job_id: &str,
        probe: impl FnOnce(&WorkerReceipt) -> ProcessLiveness,
    ) -> io::Result<WorkerObservation> {
        let _receipt_lock = acquire_worker_receipt_ownership_lock(&self.state_dir)?;
        let path = self.receipt_path(job_id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(WorkerObservation::Dead);
            }
            Err(error) => return Err(error),
        };
        let receipt: WorkerReceipt = if let Ok(receipt) = serde_json::from_slice(&bytes) {
            receipt
        } else {
            return Ok(WorkerObservation::Unknown);
        };
        if receipt.job_id != job_id {
            return Ok(WorkerObservation::Unknown);
        }
        match probe(&receipt) {
            ProcessLiveness::Alive => Ok(WorkerObservation::Alive(receipt)),
            ProcessLiveness::Dead => {
                remove_if_present(&path)?;
                Ok(WorkerObservation::Dead)
            }
            ProcessLiveness::Unknown => Ok(WorkerObservation::Unknown),
        }
    }

    fn worker_dir(&self) -> PathBuf {
        self.state_dir.join("queue-workers")
    }
    fn receipt_path(&self, job_id: &str) -> PathBuf {
        self.worker_dir().join(format!("{job_id}.json"))
    }
    fn remove_receipt_if_present(&self, job_id: &str) -> io::Result<()> {
        let _receipt_lock = acquire_worker_receipt_ownership_lock(&self.state_dir)?;
        remove_if_present(&self.receipt_path(job_id))
    }
    fn log_path(&self, job_id: &str) -> PathBuf {
        self.worker_dir().join(format!("{job_id}.log"))
    }
}

struct RunningResourceClaims {
    claims: BTreeSet<String>,
    errors: Vec<String>,
}

fn running_resource_claims(
    running: &[Job],
    store: &QueueRequestStore,
) -> Result<RunningResourceClaims, QueueRequestError> {
    let mut claims = BTreeSet::new();
    let mut errors = Vec::new();
    for job in running {
        match store.load(&job.id) {
            Ok(Some(envelope)) if envelope.job_id == job.id => {
                claims.extend(resource_claims(&envelope));
            }
            Ok(Some(envelope)) => errors.push(format!(
                "{}: queued execution request belongs to {}",
                job.id, envelope.job_id
            )),
            Ok(None) => errors.push(format!("{}: queued execution request is missing", job.id)),
            Err(error) if request_error_is_job_local(&error) => {
                errors.push(format!("{}: {error}", job.id));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(RunningResourceClaims { claims, errors })
}

fn requires_merged_ship_observation(job: &Job) -> bool {
    matches!(job.status, JobStatus::Pending | JobStatus::Running)
        && !(job.cancel_requested_at.is_some()
            && job
                .cancellation_proof
                .as_ref()
                .is_some_and(|proof| proof.cause == CancellationCause::AlreadyMerged))
}

fn request_error_is_job_local(error: &QueueRequestError) -> bool {
    matches!(
        error,
        QueueRequestError::Json(_)
            | QueueRequestError::UnsupportedSchema { .. }
            | QueueRequestError::InvalidSnapshot { .. }
    )
}

fn signal_process_tree(pid: u32) -> io::Result<Vec<u32>> {
    #[cfg(unix)]
    {
        // Stop the exact owner before snapshotting its descendants so it
        // cannot launch more work while cancellation walks the tree.
        let stopped = Command::new("/bin/kill")
            .args(["-STOP", "--", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !stopped.success() {
            return Err(io::Error::other("worker root could not be stopped"));
        }
        // Snapshot exact descendants after stopping the owner. Dispatchers
        // may create their own process groups, so signalling only `-pid` can
        // leave governed build/ctest grandchildren consuming capacity.
        let descendants = descendant_processes(pid)?;
        for descendant in descendants.iter().rev() {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &descendant.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let status = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !status?.success() {
            return Err(io::Error::other("worker process group could not be killed"));
        }
        Ok(descendants)
    }
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(io::Error::other("worker process tree could not be killed"));
        }
        Ok(Vec::new())
    }
}

#[cfg(all(test, unix))]
fn terminate_process_group(pid: u32) -> bool {
    signal_process_tree(pid).is_ok()
}

fn terminate_child_tree(child: &mut Child) -> io::Result<bool> {
    if child.try_wait()?.is_some() {
        #[cfg(unix)]
        return verify_exited_worker_group_dead(child.id());
        #[cfg(windows)]
        return Ok(false);
    }
    #[cfg_attr(
        windows,
        expect(
            unused_variables,
            reason = "Windows taskkill confirms the tree without exposing descendant PIDs"
        )
    )]
    let Ok(descendants) = signal_process_tree(child.id()) else {
        return Ok(false);
    };
    if child.wait_timeout(StdDuration::from_secs(5))?.is_some() {
        #[cfg(unix)]
        return Ok(descendants
            .iter()
            .all(|pid| process_id_liveness(*pid) == ProcessLiveness::Dead));
        #[cfg(windows)]
        return Ok(true);
    }
    // Platform fallback: never block the daemon on an unbounded wait. On
    // Windows this directly terminates the root if taskkill was unavailable;
    // on Unix it is a final exact-process escalation after the tree signal.
    let _ = child.kill();
    let root_dead = child.wait_timeout(StdDuration::from_secs(1))?.is_some();
    #[cfg(unix)]
    return Ok(root_dead
        && descendants
            .iter()
            .all(|pid| process_id_liveness(*pid) == ProcessLiveness::Dead));
    #[cfg(windows)]
    Ok(root_dead)
}

#[cfg(unix)]
fn verify_exited_worker_group_dead(process_group: u32) -> io::Result<bool> {
    // Daemon workers are created as process-group leaders. If the root raced
    // cancellation and has already exited, kill and inspect that retained
    // execution boundary before releasing its claim.
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{process_group}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let deadline = Instant::now() + StdDuration::from_secs(1);
    loop {
        let output = Command::new("/bin/ps")
            .args(["-axo", "pgid=,stat="])
            .output()?;
        if !output.status.success() {
            return Ok(false);
        }
        let live = String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            let mut fields = line.split_whitespace();
            fields.next().and_then(|value| value.parse::<u32>().ok()) == Some(process_group)
                && !fields.next().is_some_and(|state| state.starts_with('Z'))
        });
        if !live {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(StdDuration::from_millis(10));
    }
}

#[cfg(unix)]
fn descendant_processes(root: u32) -> io::Result<Vec<u32>> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid="])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("process-tree observation failed"));
    }
    let relations = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let parent = fields.next()?.parse::<u32>().ok()?;
            Some((pid, parent))
        })
        .collect::<Vec<_>>();
    let mut descendants = Vec::new();
    let mut parents = vec![root];
    while let Some(parent) = parents.pop() {
        for &(pid, observed_parent) in &relations {
            if observed_parent == parent && !descendants.contains(&pid) {
                descendants.push(pid);
                parents.push(pid);
            }
        }
    }
    Ok(descendants)
}

#[cfg(unix)]
fn process_id_liveness(pid: u32) -> ProcessLiveness {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
    else {
        return ProcessLiveness::Unknown;
    };
    if output.status.success() {
        let state = String::from_utf8_lossy(&output.stdout);
        if state.trim_start().starts_with('Z') || state.trim().is_empty() {
            ProcessLiveness::Dead
        } else {
            ProcessLiveness::Alive
        }
    } else if output.stderr.is_empty() {
        ProcessLiveness::Dead
    } else {
        ProcessLiveness::Unknown
    }
}

#[cfg_attr(
    windows,
    expect(
        clippy::unnecessary_wraps,
        reason = "the Unix implementation is fallible and callers share one cross-platform API"
    )
)]
fn worker_generation() -> io::Result<String> {
    #[cfg(unix)]
    {
        let mut bytes = [0_u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(hex::encode(bytes))
    }
    #[cfg(not(unix))]
    {
        Ok(format!(
            "{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }
}

fn admissible(envelope: &QueuedExecutionEnvelope, occupied: &BTreeSet<String>) -> bool {
    envelope.provenance.is_some()
        && resource_claims(envelope)
            .iter()
            .all(|claim| !occupied.contains(claim))
}

fn resource_claims(envelope: &QueuedExecutionEnvelope) -> BTreeSet<String> {
    let mut claims = envelope
        .resource_plan
        .exclusive_claims
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if !envelope.resource_plan.cloud_targets.is_empty() {
        claims.insert("capacity:cloud".to_owned());
    }
    claims.extend(
        envelope
            .resource_plan
            .host_pools
            .iter()
            .map(|pool| format!("capacity:host-pool:{}", pool.pool_name)),
    );
    claims.extend(
        envelope
            .resource_plan
            .vm_slots
            .iter()
            .map(|slot| format!("capacity:vm-slot:{}", slot.key)),
    );
    claims
}

fn process_liveness(receipt: &WorkerReceipt) -> ProcessLiveness {
    #[cfg(unix)]
    {
        let output = Command::new("/bin/ps")
            .args(["-p", &receipt.pid.to_string(), "-o", "command="])
            .output();
        let Ok(output) = output else {
            return ProcessLiveness::Unknown;
        };
        let command = String::from_utf8_lossy(&output.stdout);
        if output.status.success()
            && command.contains("execution-worker")
            && command.contains(&receipt.job_id)
            && command.contains(&receipt.generation)
        {
            ProcessLiveness::Alive
        } else if !output.status.success() && !output.stderr.is_empty() {
            ProcessLiveness::Unknown
        } else {
            ProcessLiveness::Dead
        }
    }
    #[cfg(windows)]
    {
        // Daemon ownership is intentionally disabled on Windows in this
        // bounded slice. Never invoke CIM/PowerShell from cross-platform unit
        // tests or pretend that process identity can be proven here.
        let _ = receipt;
        ProcessLiveness::Unknown
    }
}

/// Remove one unchanged receipt only when its exact process generation is
/// proven dead. A live/unprobeable process or a concurrently replaced receipt
/// remains fail-closed.
pub(crate) fn retire_worker_receipt_if_proven_dead(
    path: &Path,
    expected: &WorkerReceipt,
    ownership: &WorkerReceiptOwnershipGuard,
) -> io::Result<bool> {
    retire_worker_receipt_if_proven_dead_with(path, expected, ownership, || {})
}

fn retire_worker_receipt_if_proven_dead_with(
    path: &Path,
    expected: &WorkerReceipt,
    _ownership: &WorkerReceiptOwnershipGuard,
    after_dead_probe: impl FnOnce(),
) -> io::Result<bool> {
    if process_liveness(expected) != ProcessLiveness::Dead {
        return Ok(false);
    }
    after_dead_probe();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    let Ok(observed) = serde_json::from_slice::<WorkerReceipt>(&bytes) else {
        return Ok(false);
    };
    if observed != *expected {
        return Ok(false);
    }
    remove_if_present(path)?;
    Ok(true)
}

/// Verify that this exact process was fenced by the daemon before executing.
pub fn verify_worker_authority(state_dir: &Path, job_id: &str, generation: &str) -> io::Result<()> {
    let path = state_dir
        .join("queue-workers")
        .join(format!("{job_id}.json"));
    let deadline = Instant::now() + StdDuration::from_secs(3);
    loop {
        match fs::read(&path) {
            Ok(bytes) => {
                let receipt: WorkerReceipt = serde_json::from_slice(&bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if receipt.job_id == job_id
                    && receipt.generation == generation
                    && receipt.pid == std::process::id()
                {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "worker receipt does not authorize this exact process generation",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && Instant::now() < deadline => {
                thread::sleep(StdDuration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("receipt path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, value).map_err(io::Error::other)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::host_pool::{HostPoolLeaseRequest, HostPoolLeaseStore, default_lease_path};
    use crate::job::{JobKind, Priority, TargetResult, TargetStatus, ValidationMode};
    use crate::queue_request::{
        JobResourcePlan, QueueOutcomeStore, QueuedExecutionKind, QueuedExecutionOutcome,
        QueuedExecutionRequest, QueuedShipDispositionKind, QueuedShipRequest,
    };
    use crate::ship_state::ShipState;
    #[cfg(unix)]
    use crate::test_support::PROCESS_TREE_TEST_LOCK;

    fn envelope(claims: &[&str], provenance: bool) -> QueuedExecutionEnvelope {
        use crate::job::{Priority, ValidationMode};
        use crate::queue_request::{
            ExecutionProvenance, QUEUED_EXECUTION_SCHEMA_VERSION, QueuedExecutionKind,
            QueuedExecutionRequest, QueuedRunRequest,
        };
        QueuedExecutionEnvelope {
            schema_version: QUEUED_EXECUTION_SCHEMA_VERSION,
            job_id: "job".to_owned(),
            kind: QueuedExecutionKind::Run,
            cwd: PathBuf::from("/repo"),
            created_at: Utc::now(),
            execution_owner: QueuedExecutionOwner::Daemon,
            provenance: provenance.then(|| ExecutionProvenance {
                canonical_cwd: PathBuf::from("/repo"),
                repo_root: PathBuf::from("/repo"),
                repo_slug: None,
                head_sha: "abc".to_owned(),
                tree_signature: "tree".to_owned(),
                config_signature: Some("config".to_owned()),
            }),
            resource_plan: JobResourcePlan {
                exclusive_claims: claims.iter().map(ToString::to_string).collect(),
                ..JobResourcePlan::default()
            },
            request: QueuedExecutionRequest::Run(QueuedRunRequest {
                branch: "main".to_owned(),
                sha: "abc".to_owned(),
                mode: ValidationMode::Full,
                priority: Priority::Normal,
                warm_disabled: false,
                fail_fast: false,
                resume_from: None,
                targets: Vec::new(),
            }),
        }
    }

    #[test]
    fn unrelated_claims_do_not_conflict() {
        let occupied = BTreeSet::from(["repo:a".to_owned()]);
        assert!(admissible(&envelope(&["repo:b"], true), &occupied));
    }

    #[test]
    fn conflicting_claims_and_legacy_requests_fail_closed() {
        let occupied = BTreeSet::from(["repo:a".to_owned()]);
        assert!(!admissible(&envelope(&["repo:a"], true), &occupied));
        assert!(!admissible(&envelope(&[], false), &BTreeSet::new()));
    }

    #[cfg(unix)]
    #[test]
    fn pid_reuse_without_exact_worker_identity_is_rejected() {
        let receipt = WorkerReceipt {
            job_id: "not-on-this-command".to_owned(),
            generation: "unique-generation".to_owned(),
            pid: std::process::id(),
            started_at: Utc::now(),
        };
        assert_eq!(process_liveness(&receipt), ProcessLiveness::Dead);
    }

    #[cfg(unix)]
    #[test]
    fn dead_receipt_retirement_serializes_concurrent_replacement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ownership = acquire_worker_receipt_ownership_lock(temp.path()).expect("ownership");
        let path = temp.path().join("queue-workers/worker.json");
        let expected = WorkerReceipt {
            job_id: "job".to_owned(),
            generation: "dead-generation".to_owned(),
            pid: std::process::id(),
            started_at: Utc::now(),
        };
        let replacement = WorkerReceipt {
            generation: "replacement-generation".to_owned(),
            ..expected.clone()
        };
        write_json_atomic(&path, &expected).expect("expected receipt");

        let (replace_tx, replace_rx) = std::sync::mpsc::channel();
        let (published_tx, published_rx) = std::sync::mpsc::channel();
        let state_dir = temp.path().to_path_buf();
        let replacement_path = path.clone();
        let replacement_writer = replacement.clone();
        let writer = thread::spawn(move || {
            replace_rx.recv().expect("replacement boundary");
            let _ownership =
                acquire_worker_receipt_ownership_lock(&state_dir).expect("writer ownership");
            write_json_atomic(&replacement_path, &replacement_writer).expect("replacement receipt");
            published_tx.send(()).expect("published");
        });

        assert!(
            retire_worker_receipt_if_proven_dead_with(&path, &expected, &ownership, || {
                replace_tx.send(()).expect("release replacement");
                assert!(
                    published_rx
                        .recv_timeout(StdDuration::from_millis(50))
                        .is_err(),
                    "replacement writer escaped the ownership transaction"
                );
            })
            .expect("retire attempt")
        );
        drop(ownership);
        published_rx
            .recv_timeout(StdDuration::from_secs(2))
            .expect("replacement publishes after retirement unlock");
        writer.join().expect("replacement writer");
        assert_eq!(
            serde_json::from_slice::<WorkerReceipt>(&fs::read(path).expect("retained receipt"))
                .expect("receipt json"),
            replacement
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_worker_identity_fails_closed_without_process_probe() {
        let receipt = WorkerReceipt {
            job_id: "job".to_owned(),
            generation: "generation".to_owned(),
            pid: std::process::id(),
            started_at: Utc::now(),
        };
        assert_eq!(process_liveness(&receipt), ProcessLiveness::Unknown);
    }

    fn queued_job(state_dir: &Path, job_id: &str) -> Job {
        let mut job = Job::create(
            "abc",
            "main",
            vec!["local".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        )
        .with_kind(JobKind::Run);
        job.id = job_id.to_owned();
        let mut queue = Queue::new(state_dir).expect("queue");
        queue.enqueue(job.clone()).expect("enqueue");
        let mut request = envelope(&[&format!("repo:{job_id}")], true);
        request.job_id = job_id.to_owned();
        QueueRequestStore::new(state_dir)
            .expect("store")
            .save(&request)
            .expect("request");
        job
    }

    fn queued_foreground_job(state_dir: &Path, job_id: &str) -> Job {
        let job = queued_job(state_dir, job_id);
        let store = QueueRequestStore::new(state_dir).expect("store");
        let mut request = store.load(job_id).expect("load").expect("request");
        request.execution_owner = QueuedExecutionOwner::Foreground;
        request.provenance = None;
        store.save(&request).expect("foreground request");
        job
    }

    fn queued_ship_job(state_dir: &Path, job_id: &str, sha: &str) -> Job {
        let mut job = Job::create(
            sha,
            "feature/durable",
            vec!["local".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        )
        .with_kind(JobKind::Ship);
        job.id = job_id.to_owned();
        Queue::new(state_dir)
            .expect("queue")
            .enqueue(job.clone())
            .expect("enqueue");
        let mut request = envelope(&["repo:owner/repo"], true);
        request.job_id = job_id.to_owned();
        request.kind = QueuedExecutionKind::Ship;
        request.cwd = state_dir.to_path_buf();
        request.request = QueuedExecutionRequest::Ship(QueuedShipRequest {
            pr: 438,
            repo: "owner/repo".to_owned(),
            branch: "feature/durable".to_owned(),
            base_branch: "main".to_owned(),
            sha: sha.to_owned(),
            commit_subject: "durable execution".to_owned(),
            pr_url: None,
            pr_title: None,
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            advisory_targets: BTreeSet::new(),
            adopt_head: false,
            metadata_authority_receipt: None,
            targets: Vec::new(),
        });
        QueueRequestStore::new(state_dir)
            .expect("store")
            .save(&request)
            .expect("request");
        job
    }

    #[cfg(unix)]
    fn host_pool_lease_request(job_id: &str) -> HostPoolLeaseRequest {
        HostPoolLeaseRequest {
            pool_name: "local_macs".to_owned(),
            member_id: "m5".to_owned(),
            target_name: "mac".to_owned(),
            backend: "local".to_owned(),
            host: None,
            job_id: Some(job_id.to_owned()),
            branch: "feature/durable".to_owned(),
            sha: "exact-head".to_owned(),
            max_concurrency: 1,
            lease_stale_seconds: 180,
        }
    }

    #[allow(dead_code)] // Exercised by Unix controller-process tests; Windows still compiles the shared fixture.
    fn queued_metadata_job(state_dir: &Path, job_id: &str) -> Job {
        let mut job = Job::create(
            "b".repeat(40),
            "docs/fast-path",
            Vec::new(),
            ValidationMode::Full,
            Priority::Normal,
        )
        .with_kind(JobKind::Ship);
        job.id = job_id.to_owned();
        Queue::new(state_dir)
            .expect("queue")
            .enqueue(job.clone())
            .expect("enqueue");
        let mut request = envelope(&[], true);
        request.job_id = job_id.to_owned();
        request.kind = QueuedExecutionKind::Ship;
        request.resource_plan = JobResourcePlan::default();
        request.request = QueuedExecutionRequest::Ship(QueuedShipRequest {
            pr: 42,
            repo: "owner/repo".to_owned(),
            branch: "docs/fast-path".to_owned(),
            base_branch: "main".to_owned(),
            sha: "b".repeat(40),
            commit_subject: "docs".to_owned(),
            pr_url: None,
            pr_title: None,
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            advisory_targets: BTreeSet::new(),
            adopt_head: false,
            metadata_authority_receipt: Some(crate::metadata_authority::MetadataAuthorityReceipt {
                schema_version: 1,
                repository: "owner/repo".to_owned(),
                pull_request: 42,
                base_ref: "main".to_owned(),
                base_sha: "a".repeat(40),
                head_sha: "b".repeat(40),
                tree_sha: "c".repeat(40),
                observation_target: "mac".to_owned(),
                policy_digest: "d".repeat(64),
                changed_paths_digest: "e".repeat(64),
                required_checks_digest: "f".repeat(64),
                changed_paths: vec!["docs/guide.md".to_owned()],
                required_checks: vec!["docs".to_owned()],
                hosted_checks: vec![crate::metadata_authority::HostedCheckObservation {
                    name: "docs".to_owned(),
                    status: "COMPLETED".to_owned(),
                    conclusion: "SUCCESS".to_owned(),
                    producer: "app:15368".to_owned(),
                }],
            }),
            targets: Vec::new(),
        });
        QueueRequestStore::new(state_dir)
            .expect("store")
            .save(&request)
            .expect("request");
        job
    }

    fn merged_cancellations(
        state_dir: &Path,
        status: JobStatus,
        merged_head: Option<&str>,
    ) -> Vec<AlreadyMergedCancellation> {
        let jobs = Queue::new(state_dir)
            .expect("queue")
            .get_all()
            .expect("jobs");
        let store = QueueRequestStore::new(state_dir).expect("requests");
        let mut observer = AlreadyMergedObserver::from_config(&crate::config::LoadedConfig {
            data: toml::Table::new(),
            global_dir: state_dir.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: crate::config::LocalOverlaySource::None,
        });
        let fetch = |_: &str, _: u64| merged_head.map(str::to_owned);
        match status {
            JobStatus::Pending => observer.observe_pending_with(&jobs, &store, fetch),
            JobStatus::Running => observer.observe_running_with(&jobs, &store, fetch),
            _ => panic!("unsupported status"),
        }
    }

    #[cfg(unix)]
    fn fake_worker(temp: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = temp.join("fake-worker.sh");
        fs::write(&path, "#!/bin/sh\n/bin/sleep 30\n").expect("script");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("permissions");
        path
    }

    #[cfg(unix)]
    fn fake_worker_tree(temp: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = temp.join("fake-worker-tree.sh");
        let pid_path = temp.join("descendant.pid");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\n/bin/sleep 300 & echo $! > '{}'\nwait\n",
                pid_path.display()
            ),
        )
        .expect("script");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("permissions");
        path
    }

    #[cfg(unix)]
    fn process_is_running(pid: &str) -> bool {
        let output = Command::new("/bin/ps")
            .args(["-p", pid, "-o", "stat="])
            .stderr(Stdio::null())
            .output();
        output.is_ok_and(|output| {
            output.status.success()
                && !String::from_utf8_lossy(&output.stdout)
                    .trim_start()
                    .starts_with('Z')
        })
    }

    #[cfg(unix)]
    fn wait_for_live_descendant(pid_path: &Path) -> String {
        let deadline = Instant::now() + StdDuration::from_secs(15);
        loop {
            if let Ok(pid) = fs::read_to_string(pid_path) {
                let pid = pid.trim();
                if pid.parse::<u32>().is_ok_and(|pid| pid > 0) && process_is_running(pid) {
                    return pid.to_owned();
                }
            }
            assert!(
                Instant::now() < deadline,
                "fixture descendant did not publish a live nonzero PID at {}",
                pid_path.display()
            );
            thread::sleep(StdDuration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn daemon_restart_adopts_exact_live_worker_without_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "restart-adopt");
        let binary = fake_worker(temp.path());
        let mut first = ExecutionSupervisor::new(
            binary.clone(),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        first.tick().expect("first tick");
        let mut live_child = first
            .children
            .remove("restart-adopt")
            .expect("spawned child");
        drop(first);

        let mut restarted = ExecutionSupervisor::new(
            binary,
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        restarted.tick().expect("restart tick");
        assert!(
            restarted.children.is_empty(),
            "adopted worker must not be replayed"
        );
        assert_eq!(
            Queue::new(temp.path())
                .expect("queue")
                .get("restart-adopt")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running,
        );
        live_child.kill().expect("kill fixture worker");
        live_child.wait().expect("wait fixture worker");
    }

    #[cfg(unix)]
    #[test]
    fn terminal_validation_keeps_a_live_post_validation_worker_receipt() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "post-validation");
        let binary = fake_worker(temp.path());
        let mut first = ExecutionSupervisor::new(
            binary.clone(),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        first.tick().expect("first tick");
        let child = first
            .children
            .remove("post-validation")
            .expect("spawned child");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let completed = queue
            .get("post-validation")
            .expect("read")
            .expect("job")
            .complete()
            .expect("complete");
        queue.update(&completed).expect("persist completion");
        drop(first);

        let mut restarted = ExecutionSupervisor::new(
            binary,
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        restarted.tick().expect("restart tick");
        assert!(restarted.receipt_path("post-validation").exists());
        terminate_process_group(child.id());
        let mut child = child;
        let _ = child.wait();
        restarted.tick().expect("cleanup tick");
        assert!(!restarted.receipt_path("post-validation").exists());
    }

    #[cfg(unix)]
    #[test]
    fn queue_absent_recovery_reaps_stale_child_before_receipt_preflight() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut supervisor = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        fs::create_dir_all(supervisor.worker_dir()).expect("worker dir");

        let mut exited_child = Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("exited child");
        exited_child.wait().expect("wait for child");
        supervisor
            .children
            .insert("dead-owner".to_owned(), exited_child);

        let dead_receipt_path = supervisor.receipt_path("dead-owner");
        write_json_atomic(
            &dead_receipt_path,
            &WorkerReceipt {
                job_id: "dead-owner".to_owned(),
                generation: "fabricated-generation".to_owned(),
                // A live PID with the wrong exact worker identity is hostile
                // PID-reuse input and must be proven dead for this receipt.
                pid: std::process::id(),
                started_at: Utc::now(),
            },
        )
        .expect("dead receipt");

        let unknown_receipt_path = supervisor.receipt_path("unknown-owner");
        fs::write(&unknown_receipt_path, b"not-json").expect("unknown receipt");

        // This is the ordering used by tick: clear stale in-memory ownership,
        // then remove only receipts whose exact generation is proven dead.
        supervisor.reap_owned_children().expect("reap stale child");
        supervisor
            .recover_queue_absent()
            .expect("recovery preflight");

        assert!(!supervisor.children.contains_key("dead-owner"));
        assert!(
            !dead_receipt_path.exists(),
            "a provably dead receipt must be removed before detached recovery"
        );
        assert!(
            unknown_receipt_path.exists(),
            "ambiguous receipt ownership must remain fail-closed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn queue_absent_recovery_flight_preserves_orphan_worker_ownership_receipt() {
        let temp = tempfile::tempdir().expect("tempdir");
        let supervisor = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        fs::create_dir_all(supervisor.worker_dir()).expect("worker dir");
        let receipt_path = supervisor.receipt_path("preserved-owner");
        write_json_atomic(
            &receipt_path,
            &WorkerReceipt {
                job_id: "preserved-owner".to_owned(),
                generation: "preserved-generation".to_owned(),
                // The test process is live but cannot match this fabricated
                // worker identity, so the normal receipt sweep proves it dead.
                pid: std::process::id(),
                started_at: Utc::now(),
            },
        )
        .expect("receipt");

        supervisor
            .queue_absent_recovery_in_flight
            .store(true, std::sync::atomic::Ordering::Release);
        supervisor
            .sweep_terminal_receipts()
            .expect("concurrent sweep");

        assert!(
            receipt_path.exists(),
            "a queue-absence recovery must retain the orphan ownership evidence it validates"
        );

        supervisor
            .queue_absent_recovery_in_flight
            .store(false, std::sync::atomic::Ordering::Release);
        supervisor
            .sweep_terminal_receipts()
            .expect("post-recovery sweep");
        assert!(
            !receipt_path.exists(),
            "ordinary terminal receipt cleanup resumes after recovery exits"
        );
    }

    #[cfg(unix)]
    #[test]
    fn manual_cancellation_terminates_the_worker_process_group() {
        let _tree_test = PROCESS_TREE_TEST_LOCK.lock().expect("tree test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "cancel-tree");
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker_tree(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("start worker");
        let pid_path = temp.path().join("descendant.pid");
        let descendant = wait_for_live_descendant(&pid_path);

        let mut queue = Queue::new(temp.path()).expect("queue");
        let cancelled = queue
            .get("cancel-tree")
            .expect("read")
            .expect("job")
            .request_cancel_with_reason(Some("operator cancel".to_owned()))
            .expect("request cancel");
        queue.update(&cancelled).expect("persist cancel");
        queued_job(temp.path(), "replacement");
        assert_eq!(
            queue.get("cancel-tree").expect("read").expect("job").status,
            JobStatus::Running,
            "requested cancellation must retain capacity until the exact tree exits"
        );
        supervisor.tick().expect("terminate cancelled worker");

        let deadline = Instant::now() + StdDuration::from_secs(5);
        while process_is_running(descendant.trim()) && Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(10));
        }
        assert!(!process_is_running(descendant.trim()));
        assert_eq!(
            queue.get("cancel-tree").expect("read").expect("job").status,
            JobStatus::Cancelled
        );
        assert!(
            QueueOutcomeStore::new(temp.path())
                .expect("outcomes")
                .load("cancel-tree")
                .expect("load")
                .is_some(),
            "confirmed process-tree death must leave a terminal receipt"
        );
        assert_eq!(
            queue
                .get("replacement")
                .expect("read")
                .expect("replacement")
                .status,
            JobStatus::Running,
            "capacity may be reused only after exact process-tree death"
        );
        assert!(!supervisor.receipt_path("cancel-tree").exists());
        let mut replacement = supervisor.children.remove("replacement").expect("worker");
        terminate_process_group(replacement.id());
        let _ = replacement.wait();
    }

    #[cfg(unix)]
    #[test]
    fn gen42_issues_436_437_exact_merge_kills_tree_types_outcome_and_releases_capacity() {
        let _tree_test = PROCESS_TREE_TEST_LOCK.lock().expect("tree test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        queued_ship_job(temp.path(), "merged-tree", "exact-head");
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker_tree(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("start worker");
        let pid_path = temp.path().join("descendant.pid");
        let descendant = wait_for_live_descendant(&pid_path);
        let lease_store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        let merged_lease = lease_store
            .acquire(&host_pool_lease_request("merged-tree"))
            .expect("acquire merged-tree lease")
            .expect("merged-tree lease");
        assert!(
            lease_store
                .acquire(&host_pool_lease_request("replacement"))
                .expect("capacity probe before death")
                .is_none(),
            "host-pool capacity must remain fenced while the worker tree is alive"
        );
        queued_job(temp.path(), "replacement");
        let cancellations =
            merged_cancellations(temp.path(), JobStatus::Running, Some("exact-head"));
        assert_eq!(cancellations.len(), 1);
        supervisor
            .apply_merge_cancellations(Vec::new(), cancellations)
            .expect("request exact-head cancellation");
        assert_eq!(
            Queue::new(temp.path())
                .expect("queue")
                .get("merged-tree")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running,
            "capacity remains claimed until tree death is proven"
        );
        supervisor.tick().expect("terminate exact tree");
        let deadline = Instant::now() + StdDuration::from_secs(5);
        while process_is_running(descendant.trim()) && Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(10));
        }
        assert!(!process_is_running(descendant.trim()));
        let mut queue = Queue::new(temp.path()).expect("queue");
        let cancelled = queue.get("merged-tree").expect("read").expect("job");
        assert_eq!(cancelled.status, JobStatus::Cancelled);
        assert_eq!(
            cancelled.cancellation_reason.as_deref(),
            Some(crate::queue::ALREADY_MERGED_CANCEL_REASON)
        );
        let outcome = QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .load("merged-tree")
            .expect("load")
            .expect("merged-tree outcome");
        let QueuedExecutionOutcome::Ship {
            post_validation: Some(disposition),
            ..
        } = outcome
        else {
            panic!("expected ship outcome with terminal disposition");
        };
        assert_eq!(disposition.kind, QueuedShipDispositionKind::AlreadyMerged);
        assert_eq!(disposition.exit_code, 0);
        assert!(
            lease_store
                .leases()
                .expect("leases after cancellation")
                .iter()
                .all(|lease| lease.lease_id != merged_lease.lease_id),
            "confirmed process-tree death must durably release the killed worker lease"
        );
        let replacement_lease = lease_store
            .acquire(&host_pool_lease_request("replacement"))
            .expect("capacity probe after death")
            .expect("replacement capacity");
        assert_eq!(
            queue.get("replacement").expect("read").expect("job").status,
            JobStatus::Running
        );
        assert!(
            lease_store
                .release(&replacement_lease.lease_id)
                .expect("cleanup lease")
        );
        let mut replacement = supervisor.children.remove("replacement").expect("worker");
        terminate_process_group(replacement.id());
        let _ = replacement.wait();
    }

    #[cfg(unix)]
    #[test]
    fn gen42_issue_437_dead_root_only_cannot_release_lease_or_acknowledge_cancellation() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_ship_job(temp.path(), "killed-worker", "exact-head");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        let running = queue
            .start_pending_jobs_for_drain(&lock, &["killed-worker".to_owned()])
            .expect("start")
            .remove(0)
            .request_cancel_with_reason(Some(crate::queue::ALREADY_MERGED_CANCEL_REASON.to_owned()))
            .expect("request cancel");
        drop(lock);
        queue
            .update(&running)
            .expect("persist cancellation request");

        let mut dead_worker = Command::new("/usr/bin/true").spawn().expect("dead worker");
        let dead_pid = dead_worker.id();
        dead_worker.wait().expect("reap dead worker");
        let receipt = WorkerReceipt {
            job_id: running.id.clone(),
            generation: "killed-before-drop".to_owned(),
            pid: dead_pid,
            started_at: Utc::now(),
        };
        let supervisor = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        write_json_atomic(&supervisor.receipt_path(&running.id), &receipt).expect("dead receipt");
        let lease_store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        lease_store
            .acquire(&host_pool_lease_request(&running.id))
            .expect("acquire orphan lease")
            .expect("orphan lease");

        let mut restarted = supervisor;
        restarted
            .tick()
            .expect("fail-closed restart reconciliation");

        let preserved = queue.get(&running.id).expect("read").expect("job");
        assert_eq!(preserved.status, JobStatus::Running);
        assert_eq!(
            preserved.cancellation_reason.as_deref(),
            Some(crate::queue::ALREADY_MERGED_CANCEL_REASON)
        );
        assert_eq!(lease_store.leases().expect("leases").len(), 1);
    }

    #[cfg(unix)]
    #[derive(Clone, Copy, Debug)]
    enum TerminationCrashBoundary {
        FrozenBeforeTreeDeath,
        TreeDeadBeforeLeaseRelease,
        LeaseReleasedBeforeMarker,
        MarkerBeforeQueueFinalization,
    }

    #[cfg(unix)]
    #[test]
    fn gen42_issue_437_cancel_restart_completes_every_durable_crash_boundary() {
        for boundary in [
            TerminationCrashBoundary::FrozenBeforeTreeDeath,
            TerminationCrashBoundary::TreeDeadBeforeLeaseRelease,
            TerminationCrashBoundary::LeaseReleasedBeforeMarker,
            TerminationCrashBoundary::MarkerBeforeQueueFinalization,
        ] {
            assert_termination_crash_recovers(TerminationAction::Cancel, boundary, true);
        }
    }

    #[cfg(unix)]
    #[test]
    fn receiptless_cancel_restart_completes_every_durable_crash_boundary() {
        for boundary in [
            TerminationCrashBoundary::FrozenBeforeTreeDeath,
            TerminationCrashBoundary::TreeDeadBeforeLeaseRelease,
            TerminationCrashBoundary::LeaseReleasedBeforeMarker,
            TerminationCrashBoundary::MarkerBeforeQueueFinalization,
        ] {
            assert_termination_crash_recovers(TerminationAction::Cancel, boundary, false);
        }
    }

    #[cfg(unix)]
    #[test]
    fn gen42_issue_437_defer_restart_completes_every_durable_crash_boundary() {
        for boundary in [
            TerminationCrashBoundary::FrozenBeforeTreeDeath,
            TerminationCrashBoundary::TreeDeadBeforeLeaseRelease,
            TerminationCrashBoundary::LeaseReleasedBeforeMarker,
            TerminationCrashBoundary::MarkerBeforeQueueFinalization,
        ] {
            assert_termination_crash_recovers(TerminationAction::Defer, boundary, true);
        }
    }

    #[cfg(unix)]
    #[test]
    fn receiptless_defer_restart_completes_every_durable_crash_boundary() {
        for boundary in [
            TerminationCrashBoundary::FrozenBeforeTreeDeath,
            TerminationCrashBoundary::TreeDeadBeforeLeaseRelease,
            TerminationCrashBoundary::LeaseReleasedBeforeMarker,
            TerminationCrashBoundary::MarkerBeforeQueueFinalization,
        ] {
            assert_termination_crash_recovers(TerminationAction::Defer, boundary, false);
        }
    }

    #[cfg(unix)]
    #[test]
    fn receiptless_recovery_still_refuses_a_present_replacement_generation() {
        let _tree_test = PROCESS_TREE_TEST_LOCK.lock().expect("tree test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let job_id = "replacement-generation";
        queued_ship_job(temp.path(), job_id, "exact-head");
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker_tree(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("start exact worker");
        wait_for_live_descendant(&temp.path().join("descendant.pid"));
        Queue::new(temp.path())
            .expect("queue")
            .request_cancel(job_id, Some("operator cancel".to_owned()))
            .expect("request cancel")
            .expect("running job");
        let WorkerObservation::Alive(exact_receipt) = supervisor
            .observe_cancellation_receipt(job_id)
            .expect("observe exact worker")
        else {
            panic!("expected live exact worker receipt");
        };
        TerminationStore::new(temp.path())
            .begin(&exact_receipt, TerminationAction::Cancel)
            .expect("freeze exact tree");
        let replacement = WorkerReceipt {
            job_id: job_id.to_owned(),
            generation: "replacement-generation".to_owned(),
            pid: std::process::id(),
            started_at: Utc::now(),
        };
        write_json_atomic(&supervisor.receipt_path(job_id), &replacement)
            .expect("publish replacement receipt");

        let error = supervisor
            .complete_worker_termination(job_id, TerminationAction::Cancel)
            .expect_err("replacement generation must fail closed");
        assert!(error.to_string().contains("receipt generation changed"));
        assert_eq!(
            Queue::new(temp.path())
                .expect("queue")
                .get(job_id)
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running
        );
        assert_eq!(
            serde_json::from_slice::<WorkerReceipt>(
                &fs::read(supervisor.receipt_path(job_id)).expect("replacement retained")
            )
            .expect("receipt json"),
            replacement
        );

        // Restore the exact receipt solely to finish the frozen test worker;
        // production recovery must never erase or adopt the replacement.
        write_json_atomic(&supervisor.receipt_path(job_id), &exact_receipt)
            .expect("restore exact receipt for cleanup");
        let transaction = supervisor
            .complete_worker_termination(job_id, TerminationAction::Cancel)
            .expect("finish exact transaction")
            .expect("tree-death proof");
        assert_eq!(transaction.phase, TerminationPhase::LeasesReleased);
        supervisor
            .cleanup_termination_transaction(&transaction)
            .expect("cleanup transaction");
    }

    #[cfg(unix)]
    #[test]
    fn gen42_issue_437_cancel_promotes_released_defer_transaction_on_restart() {
        let _tree_test = PROCESS_TREE_TEST_LOCK.lock().expect("tree test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "defer-then-cancel");
        let mut original = ExecutionSupervisor::new(
            fake_worker_tree(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        original.tick().expect("start worker");
        wait_for_live_descendant(&temp.path().join("descendant.pid"));
        let lease_store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        lease_store
            .acquire(&host_pool_lease_request("defer-then-cancel"))
            .expect("acquire lease")
            .expect("lease");
        let mut queue = Queue::new(temp.path()).expect("queue");
        queue
            .requeue_deferred_daemon_worker(QueueDeferredRequeue {
                job_id: "defer-then-cancel".to_owned(),
                reason: "capacity unavailable".to_owned(),
                defer_until: Some(Utc::now() + Duration::minutes(1)),
            })
            .expect("defer")
            .expect("running job");
        let WorkerObservation::Alive(receipt) = original
            .observe_cancellation_receipt("defer-then-cancel")
            .expect("observe")
        else {
            panic!("expected exact worker");
        };
        let store = TerminationStore::new(temp.path());
        let mut transaction = store
            .begin(&receipt, TerminationAction::Defer)
            .expect("freeze");
        let mut child = original
            .children
            .remove("defer-then-cancel")
            .expect("child");
        assert!(
            store
                .prove_tree_dead(&mut transaction, Some(&mut child))
                .expect("tree dead")
        );
        lease_store
            .release_for_job("defer-then-cancel")
            .expect("release");
        store
            .mark_leases_released(&mut transaction)
            .expect("released marker");
        queue
            .request_cancel("defer-then-cancel", Some("operator cancel".to_owned()))
            .expect("cancel")
            .expect("running cancellation");
        drop(original);

        let mut restarted = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        restarted.tick().expect("promote and finalize cancellation");

        assert_eq!(
            queue
                .get("defer-then-cancel")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Cancelled
        );
        assert!(store.list().expect("transactions").is_empty());
        assert!(lease_store.leases().expect("leases").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn gen42_issue_437_direct_defer_cleanup_preserves_replacement_generation() {
        assert_defer_cleanup_preserves_replacement_generation(false);
    }

    #[cfg(unix)]
    #[test]
    fn gen42_issue_437_restart_defer_cleanup_preserves_replacement_generation() {
        assert_defer_cleanup_preserves_replacement_generation(true);
    }

    #[cfg(unix)]
    fn assert_defer_cleanup_preserves_replacement_generation(restart_cleanup: bool) {
        let _tree_test = PROCESS_TREE_TEST_LOCK.lock().expect("tree test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let job_id = "defer-generation-cas";
        queued_job(temp.path(), job_id);
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker_tree(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("start generation A");
        wait_for_live_descendant(&temp.path().join("descendant.pid"));
        let mut queue = Queue::new(temp.path()).expect("queue");
        queue
            .requeue_deferred_daemon_worker(QueueDeferredRequeue {
                job_id: job_id.to_owned(),
                reason: "capacity unavailable".to_owned(),
                defer_until: Some(Utc::now() + Duration::minutes(1)),
            })
            .expect("defer generation A")
            .expect("running job");
        let replacement = WorkerReceipt {
            job_id: job_id.to_owned(),
            generation: "generation-b".to_owned(),
            pid: std::process::id(),
            started_at: Utc::now(),
        };
        if restart_cleanup {
            let transaction = supervisor
                .complete_worker_termination(job_id, TerminationAction::Defer)
                .expect("terminate generation A")
                .expect("released transaction");
            queue
                .finalize_deferred_daemon_worker(job_id)
                .expect("finalize defer")
                .expect("pending job");
            write_json_atomic(&supervisor.receipt_path(job_id), &replacement)
                .expect("publish generation B");
            assert_eq!(transaction.phase, TerminationPhase::LeasesReleased);
            drop(supervisor);
            let restarted = ExecutionSupervisor::new(
                PathBuf::from("/does/not/exist"),
                RuntimeMode::Isolated,
                temp.path().into(),
                temp.path().into(),
            );
            restarted
                .reconcile_finalized_termination_transactions()
                .expect("restart cleanup");
        } else {
            let receipt_path = supervisor.receipt_path(job_id);
            supervisor
                .terminate_deferred_workers_with_cleanup_hook(|observed_job, transaction| {
                    assert_eq!(observed_job, job_id);
                    assert_eq!(transaction.phase, TerminationPhase::LeasesReleased);
                    write_json_atomic(&receipt_path, &replacement)
                        .expect("publish generation B between finalize and cleanup");
                })
                .expect("direct defer finalization");
        }

        let retained: WorkerReceipt = serde_json::from_slice(
            &fs::read(temp.path().join(format!("queue-workers/{job_id}.json")))
                .expect("generation B retained"),
        )
        .expect("replacement receipt");
        assert_eq!(retained, replacement);
        assert!(
            TerminationStore::new(temp.path())
                .list()
                .expect("transactions")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_lines)]
    fn assert_termination_crash_recovers(
        action: TerminationAction,
        boundary: TerminationCrashBoundary,
        retain_receipt: bool,
    ) {
        let _tree_test = PROCESS_TREE_TEST_LOCK.lock().expect("tree test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let job_id = match action {
            TerminationAction::Cancel => {
                queued_ship_job(temp.path(), "crash-cancel", "exact-head");
                "crash-cancel"
            }
            TerminationAction::Defer => {
                queued_job(temp.path(), "crash-defer");
                "crash-defer"
            }
        };
        let binary = fake_worker_tree(temp.path());
        let mut original = ExecutionSupervisor::new(
            binary,
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        original.tick().expect("start worker");
        wait_for_live_descendant(&temp.path().join("descendant.pid"));
        let lease_store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        lease_store
            .acquire(&host_pool_lease_request(job_id))
            .expect("acquire worker lease")
            .expect("worker lease");
        let mut queue = Queue::new(temp.path()).expect("queue");
        match action {
            TerminationAction::Cancel => {
                queue
                    .request_cancel_with_proof(
                        job_id,
                        Some(crate::queue::ALREADY_MERGED_CANCEL_REASON.to_owned()),
                        Some(CancellationProof {
                            cause: CancellationCause::AlreadyMerged,
                            repository: "owner/repo".to_owned(),
                            pull_request: 438,
                            head_sha: "exact-head".to_owned(),
                        }),
                    )
                    .expect("request cancellation")
                    .expect("running job");
            }
            TerminationAction::Defer => {
                queue
                    .requeue_deferred_daemon_worker(QueueDeferredRequeue {
                        job_id: job_id.to_owned(),
                        reason: "capacity unavailable".to_owned(),
                        defer_until: Some(Utc::now() + Duration::minutes(1)),
                    })
                    .expect("request deferral")
                    .expect("running job");
            }
        }
        let WorkerObservation::Alive(receipt) = original
            .observe_cancellation_receipt(job_id)
            .expect("observe exact worker")
        else {
            panic!("expected live exact worker receipt");
        };
        let store = TerminationStore::new(temp.path());
        let mut transaction = store.begin(&receipt, action).expect("freeze tree");
        if !matches!(boundary, TerminationCrashBoundary::FrozenBeforeTreeDeath) {
            let mut child = original.children.remove(job_id).expect("owned child");
            assert!(
                store
                    .prove_tree_dead(&mut transaction, Some(&mut child))
                    .expect("prove tree dead")
            );
        }
        if matches!(
            boundary,
            TerminationCrashBoundary::LeaseReleasedBeforeMarker
                | TerminationCrashBoundary::MarkerBeforeQueueFinalization
        ) {
            assert_eq!(
                lease_store.release_for_job(job_id).expect("release lease"),
                1
            );
        }
        if matches!(
            boundary,
            TerminationCrashBoundary::MarkerBeforeQueueFinalization
        ) {
            store
                .mark_leases_released(&mut transaction)
                .expect("released marker");
        }
        if !retain_receipt {
            remove_if_present(&original.receipt_path(job_id)).expect("remove exact receipt");
            queued_job(temp.path(), "replacement");
        }
        drop(original);

        let mut restarted = ExecutionSupervisor::new(
            if retain_receipt {
                PathBuf::from("/does/not/exist")
            } else {
                fake_worker(temp.path())
            },
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        restarted.tick().expect("restart completes transaction");

        assert!(lease_store.leases().expect("leases").is_empty());
        assert!(store.list().expect("transactions").is_empty());
        let job = queue.get(job_id).expect("read").expect("job");
        match action {
            TerminationAction::Cancel => {
                assert_eq!(job.status, JobStatus::Cancelled);
                let outcome = QueueOutcomeStore::new(temp.path())
                    .expect("outcomes")
                    .load(job_id)
                    .expect("load")
                    .expect("outcome");
                let QueuedExecutionOutcome::Ship {
                    post_validation: Some(disposition),
                    ..
                } = outcome
                else {
                    panic!("expected ship outcome");
                };
                assert_eq!(disposition.kind, QueuedShipDispositionKind::AlreadyMerged);
            }
            TerminationAction::Defer => assert_eq!(job.status, JobStatus::Pending),
        }
        if !retain_receipt {
            assert_eq!(
                queue
                    .get("replacement")
                    .expect("read replacement")
                    .expect("replacement job")
                    .status,
                JobStatus::Running
            );
            assert!(restarted.children.contains_key("replacement"));
            restarted.tick().expect("idempotent second tick");
            assert_eq!(
                queue
                    .get("replacement")
                    .expect("read replacement")
                    .expect("replacement job")
                    .status,
                JobStatus::Running
            );
            let mut replacement = restarted
                .children
                .remove("replacement")
                .expect("replacement worker");
            terminate_process_group(replacement.id());
            let _ = replacement.wait();
        }
    }

    #[test]
    fn exact_head_pending_cancels_but_wrong_head_and_open_do_not() {
        for (name, observed, expected) in [
            ("exact", Some("exact-head"), true),
            ("wrong", Some("different-head"), false),
            ("open", None, false),
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            queued_ship_job(temp.path(), name, "exact-head");
            let cancellations = merged_cancellations(temp.path(), JobStatus::Pending, observed);
            assert_eq!(!cancellations.is_empty(), expected, "{name}");
        }
    }

    #[test]
    fn exact_merged_cancellation_proof_stops_repeated_remote_observation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pending = queued_ship_job(temp.path(), "proven-merged", "exact-head");
        assert!(requires_merged_ship_observation(&pending));

        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        queue
            .start_pending_jobs_for_drain(&lock, std::slice::from_ref(&pending.id))
            .expect("start");
        drop(lock);
        let proven = queue
            .request_cancel_with_proof(
                &pending.id,
                Some(crate::queue::ALREADY_MERGED_CANCEL_REASON.to_owned()),
                Some(CancellationProof {
                    cause: CancellationCause::AlreadyMerged,
                    repository: "owner/repo".to_owned(),
                    pull_request: 438,
                    head_sha: "exact-head".to_owned(),
                }),
            )
            .expect("request cancellation")
            .expect("running job");

        assert!(!requires_merged_ship_observation(&proven));
    }

    #[cfg(unix)]
    #[test]
    fn poisoned_envelope_cannot_select_auth_helper_before_provenance_fence() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_ship_job(temp.path(), "poisoned", "expected-head");
        let attacker = temp.path().join("attacker");
        fs::create_dir_all(attacker.join(".shipyard")).expect("attacker config dir");
        let marker = temp.path().join("helper-ran");
        fs::write(
            attacker.join(".shipyard/config.toml"),
            format!(
                "[github.auth]\nsource = \"command\"\ntoken_command = [\"/usr/bin/touch\", \"{}\"]\n",
                marker.display()
            ),
        )
        .expect("poison config");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let mut envelope = store.load("poisoned").expect("load").expect("request");
        envelope.cwd = attacker.clone();
        let provenance = envelope.provenance.as_mut().expect("provenance");
        provenance.canonical_cwd = attacker.clone();
        provenance.repo_root = attacker;
        store.save(&envelope).expect("poisoned envelope");

        let mut supervisor = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor
            .observe_merged_ship_jobs()
            .expect("safe observation");
        assert!(!marker.exists(), "untrusted token helper executed");
        assert_eq!(
            Queue::new(temp.path())
                .expect("queue")
                .get("poisoned")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Pending
        );
    }

    #[cfg(unix)]
    #[test]
    fn changed_config_cannot_invoke_auth_helper_before_signature_fence() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let global = temp.path().join("global");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&global).expect("global");
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        fs::write(repo.join("tracked"), "stable").expect("tracked");
        git(&["add", "."]);
        git(&["commit", "-qm", "initial"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ]);
        let head = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .expect("head")
                .stdout,
        )
        .expect("utf8");
        let head = head.trim();
        fs::write(
            global.join("config.toml"),
            "[github.auth]\nsource = \"command\"\ntoken_command = [\"/usr/bin/printf\", \"token\"]\n",
        )
        .expect("original config");
        let original = crate::config::LoadedConfig::load_from_cwd_with_global_dir(
            RuntimeMode::Isolated,
            &repo,
            global.clone(),
        )
        .expect("load original config");
        queued_ship_job(temp.path(), "config-drift", head);
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let mut envelope = store.load("config-drift").expect("load").expect("request");
        let provenance = crate::queue_request::ExecutionProvenance::capture_with_config(
            &repo,
            Some("owner/repo"),
            head,
            &original,
        )
        .expect("provenance");
        let original_signature = provenance
            .config_signature
            .clone()
            .expect("config signature");
        envelope.cwd.clone_from(&provenance.canonical_cwd);
        envelope.provenance = Some(provenance);
        store.save(&envelope).expect("valid envelope");

        let marker = temp.path().join("changed-helper-ran");
        fs::write(
            global.join("config.toml"),
            format!(
                "[github.auth]\nsource = \"command\"\ntoken_command = [\"/usr/bin/touch\", \"{}\"]\n",
                marker.display()
            ),
        )
        .expect("changed config");
        let mut supervisor = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            global,
            temp.path().into(),
        );
        supervisor.merge_observers.insert(
            repo.clone(),
            (
                AlreadyMergedObserver::from_config(&original),
                original_signature,
            ),
        );
        supervisor
            .observe_merged_ship_jobs()
            .expect("safe observation");
        assert!(!marker.exists(), "changed token helper executed");
        assert_eq!(
            Queue::new(temp.path())
                .expect("queue")
                .get("config-drift")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Pending
        );
    }

    #[cfg(unix)]
    #[test]
    fn gen42_issue_437_restart_kills_adopted_tree_before_releasing_lease_and_capacity() {
        let _tree_test = PROCESS_TREE_TEST_LOCK.lock().expect("tree test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "adopted-cancel-tree");
        let binary = fake_worker_tree(temp.path());
        let mut original = ExecutionSupervisor::new(
            binary.clone(),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        original.tick().expect("start worker");
        let pid_path = temp.path().join("descendant.pid");
        let descendant = wait_for_live_descendant(&pid_path);
        let lease_store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        lease_store
            .acquire(&host_pool_lease_request("adopted-cancel-tree"))
            .expect("acquire adopted lease")
            .expect("adopted lease");
        drop(original);

        let mut queue = Queue::new(temp.path()).expect("queue");
        let cancelled = queue
            .get("adopted-cancel-tree")
            .expect("read")
            .expect("job")
            .request_cancel_with_reason(Some("operator cancel after restart".to_owned()))
            .expect("request cancel");
        queue.update(&cancelled).expect("persist cancel");
        queued_job(temp.path(), "adopted-replacement");

        let mut restarted = ExecutionSupervisor::new(
            binary,
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        restarted.tick().expect("terminate adopted tree");
        assert!(!process_is_running(descendant.trim()));
        assert!(
            lease_store.leases().expect("leases").is_empty(),
            "the restarted supervisor must release only after exact adopted-tree death"
        );
        assert_eq!(
            queue
                .get("adopted-cancel-tree")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Cancelled
        );
        assert_eq!(
            queue
                .get("adopted-replacement")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running
        );
        let mut replacement = restarted
            .children
            .remove("adopted-replacement")
            .expect("replacement child");
        terminate_process_group(replacement.id());
        let _ = replacement.wait();
        restarted.tick().expect("idempotent post-release tick");
        assert!(lease_store.leases().expect("leases").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn malformed_and_legacy_pending_jobs_do_not_block_valid_work() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "malformed");
        fs::write(
            QueueRequestStore::new(temp.path())
                .expect("store")
                .path_for("malformed"),
            b"{",
        )
        .expect("malformed request");
        queued_job(temp.path(), "legacy");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let mut legacy = store.load("legacy").expect("load").expect("request");
        legacy.provenance = None;
        store.save(&legacy).expect("legacy request");
        queued_job(temp.path(), "valid");

        let mut supervisor = ExecutionSupervisor::new(
            fake_worker(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("tick");
        let mut queue = Queue::new(temp.path()).expect("queue");
        assert_eq!(
            queue.get("malformed").expect("read").expect("job").status,
            JobStatus::Cancelled
        );
        assert_eq!(
            queue.get("legacy").expect("read").expect("job").status,
            JobStatus::Cancelled
        );
        assert_eq!(
            queue.get("valid").expect("read").expect("job").status,
            JobStatus::Running
        );
        let mut child = supervisor.children.remove("valid").expect("valid worker");
        terminate_process_group(child.id());
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn running_metadata_controller_does_not_consume_native_worker_slot() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_metadata_job(temp.path(), "metadata");
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("start metadata controller");
        queued_job(temp.path(), "native");

        supervisor.admit_pending().expect("admit native worker");

        let mut queue = Queue::new(temp.path()).expect("queue");
        assert_eq!(
            queue.get("metadata").expect("read").expect("job").status,
            JobStatus::Running
        );
        assert_eq!(
            queue.get("native").expect("read").expect("job").status,
            JobStatus::Running
        );
        for job_id in ["metadata", "native"] {
            let mut child = supervisor.children.remove(job_id).expect("controller");
            terminate_process_group(child.id());
            let _ = child.wait();
        }
    }

    #[cfg(unix)]
    #[test]
    fn malformed_pending_job_is_cancelled_while_worker_capacity_is_full() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "running");
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("start running worker");
        queued_job(temp.path(), "malformed");
        fs::write(
            QueueRequestStore::new(temp.path())
                .expect("store")
                .path_for("malformed"),
            b"{",
        )
        .expect("malformed request");

        supervisor.admit_pending().expect("scan full queue");

        let mut queue = Queue::new(temp.path()).expect("queue");
        assert_eq!(
            queue.get("running").expect("read").expect("job").status,
            JobStatus::Running
        );
        assert_eq!(
            queue.get("malformed").expect("read").expect("job").status,
            JobStatus::Cancelled
        );
        let mut child = supervisor.children.remove("running").expect("worker");
        terminate_process_group(child.id());
        let _ = child.wait();
    }

    #[cfg(unix)]
    fn assert_unknown_running_request_blocks_admission(
        mutate_request: impl FnOnce(&QueueRequestStore, &str),
    ) {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "running");
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("start running worker");
        queued_job(temp.path(), "pending");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        mutate_request(&store, "running");

        supervisor.tick().expect("fail-closed admission tick");

        let mut queue = Queue::new(temp.path()).expect("queue");
        assert_eq!(
            queue.get("running").expect("read").expect("running").status,
            JobStatus::Running
        );
        assert_eq!(
            queue.get("pending").expect("read").expect("pending").status,
            JobStatus::Pending
        );
        let mut child = supervisor.children.remove("running").expect("worker");
        terminate_process_group(child.id());
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn missing_running_request_fails_closed_without_admitting_pending_work() {
        assert_unknown_running_request_blocks_admission(|store, job_id| {
            store.delete(job_id).expect("delete running request");
        });
    }

    #[cfg(unix)]
    #[test]
    fn corrupt_running_request_fails_closed_without_admitting_pending_work() {
        assert_unknown_running_request_blocks_admission(|store, job_id| {
            fs::write(store.path_for(job_id), b"{").expect("corrupt running request");
        });
    }

    #[cfg(unix)]
    #[test]
    fn mismatched_running_request_fails_closed_without_admitting_pending_work() {
        assert_unknown_running_request_blocks_admission(|store, job_id| {
            let mut swapped = store.load(job_id).expect("load").expect("request");
            swapped.job_id = "different-job".to_owned();
            fs::write(
                store.path_for(job_id),
                serde_json::to_vec(&swapped).expect("serialize"),
            )
            .expect("swap running request");
        });
    }

    #[cfg(unix)]
    #[test]
    fn daemon_does_not_admit_or_cancel_foreground_pending_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_foreground_job(temp.path(), "foreground");
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );

        supervisor.tick().expect("tick");

        let spawned = supervisor.children.remove("foreground");
        if let Some(mut child) = spawned {
            terminate_process_group(child.id());
            let _ = child.wait();
            panic!("daemon spawned a foreground-owned request");
        }
        assert_eq!(
            Queue::new(temp.path())
                .expect("queue")
                .get("foreground")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Pending
        );
    }

    #[test]
    fn daemon_does_not_terminalize_foreground_running_without_worker_receipt() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_foreground_job(temp.path(), "foreground-running");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        queue
            .start_pending_jobs_for_drain(&lock, &["foreground-running".to_owned()])
            .expect("start");
        drop(lock);
        let mut supervisor = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );

        supervisor.tick().expect("tick");

        assert_eq!(
            queue
                .get("foreground-running")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running
        );
    }

    #[test]
    fn stale_running_without_worker_becomes_uncertain_and_durable() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "lost-worker");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        let mut running = queue
            .start_pending_jobs_for_drain(&lock, &["lost-worker".to_owned()])
            .expect("start")
            .remove(0);
        running.started_at = Some(Utc::now() - Duration::minutes(4));
        queue.update(&running).expect("persist running job");
        drop(lock);

        let mut supervisor = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("tick");
        let completed = queue.get("lost-worker").expect("read").expect("job");
        assert_eq!(completed.status, JobStatus::Completed);
        assert!(
            completed
                .results
                .values()
                .all(|result| result.failure_class.as_deref() == Some("UNCERTAIN"))
        );
        assert!(
            QueueOutcomeStore::new(temp.path())
                .expect("outcomes")
                .load("lost-worker")
                .expect("load")
                .is_some()
        );
    }

    #[test]
    fn daemon_restart_repairs_missing_terminal_outcome() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "repair-outcome");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        queue
            .start_pending_jobs_for_drain(&lock, &["repair-outcome".to_owned()])
            .expect("start");
        drop(lock);
        queue
            .complete_running_uncertain("repair-outcome", "lost owner")
            .expect("terminalize");
        assert!(
            QueueOutcomeStore::new(temp.path())
                .expect("outcomes")
                .load("repair-outcome")
                .expect("load")
                .is_none()
        );

        let mut restarted = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        restarted.tick().expect("repair tick");
        assert!(
            QueueOutcomeStore::new(temp.path())
                .expect("outcomes")
                .load("repair-outcome")
                .expect("load")
                .is_some()
        );
    }

    #[test]
    fn daemon_restart_repairs_terminal_ship_missing_post_validation_disposition() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_ship_job(temp.path(), "repair-post-validation", "validated-head");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        let running = queue
            .start_pending_jobs_for_drain(&lock, &["repair-post-validation".to_owned()])
            .expect("start")
            .remove(0);
        drop(lock);
        let completed = running
            .with_result(TargetResult::new(
                "local",
                "macos-arm64",
                TargetStatus::Pass,
                "local",
            ))
            .complete()
            .expect("complete");
        queue.update(&completed).expect("persist terminal job");
        QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .save(&QueuedExecutionOutcome::ship(
                completed.id.clone(),
                438,
                ShipState::new(
                    438,
                    "owner/repo",
                    "feature/durable",
                    "main",
                    "validated-head",
                    "preliminary-policy",
                ),
                false,
            ))
            .expect("preliminary outcome without disposition");

        let mut restarted = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        restarted.tick().expect("repair tick");

        let repaired = QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .load(&completed.id)
            .expect("load")
            .expect("repaired outcome");
        let QueuedExecutionOutcome::Ship {
            ship_state,
            post_validation,
            ..
        } = repaired
        else {
            panic!("expected ship outcome");
        };
        assert_eq!(ship_state.evidence_snapshot["local"], "pass");
        assert_eq!(
            post_validation.expect("recovered disposition").kind,
            QueuedShipDispositionKind::PostValidationOperationalFailure
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_and_periodic_reconcile_terminalize_ownerless_job_and_free_capacity() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "ownerless");
        let request_store = QueueRequestStore::new(temp.path()).expect("request store");
        let request = request_store
            .load("ownerless")
            .expect("load request")
            .expect("ownerless request");
        let mut legacy_request = serde_json::to_value(request).expect("serialize request");
        legacy_request
            .as_object_mut()
            .expect("request object")
            .remove("execution_owner");
        fs::write(
            request_store.path_for("ownerless"),
            serde_json::to_vec(&legacy_request).expect("encode legacy request"),
        )
        .expect("write legacy request");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        let mut ownerless = queue
            .start_pending_jobs_for_drain(&lock, &["ownerless".to_owned()])
            .expect("start")
            .remove(0);
        ownerless.started_at = Some(Utc::now() - Duration::days(4));
        queue.update(&ownerless).expect("age orphan");
        drop(lock);
        queued_job(temp.path(), "replacement");
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );

        // ExecutionSupervisor ticks independently of registered repository
        // subscriptions, so this models both daemon startup and its periodic
        // empty-repository reconcile loop.
        supervisor.tick().expect("startup reconcile");
        supervisor.tick().expect("periodic reconcile");

        let completed = queue.get("ownerless").expect("read").expect("orphan");
        assert_eq!(completed.status, JobStatus::Completed);
        assert!(
            completed
                .results
                .values()
                .all(|result| result.failure_class.as_deref() == Some("UNCERTAIN"))
        );
        assert!(
            QueueOutcomeStore::new(temp.path())
                .expect("outcomes")
                .load("ownerless")
                .expect("load")
                .is_some(),
            "owner death must leave an explicit durable terminal receipt"
        );
        assert_eq!(
            queue
                .get("replacement")
                .expect("read")
                .expect("replacement")
                .status,
            JobStatus::Running,
            "orphan recovery must free capacity in the same tick"
        );
        let mut child = supervisor
            .children
            .remove("replacement")
            .expect("replacement worker");
        terminate_process_group(child.id());
        let _ = child.wait();
    }

    #[test]
    fn fresh_legacy_daemon_job_without_receipt_is_preserved() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "fresh-legacy");
        let request_store = QueueRequestStore::new(temp.path()).expect("request store");
        let request = request_store
            .load("fresh-legacy")
            .expect("load request")
            .expect("legacy request");
        let mut legacy_request = serde_json::to_value(request).expect("serialize request");
        legacy_request
            .as_object_mut()
            .expect("request object")
            .remove("execution_owner");
        fs::write(
            request_store.path_for("fresh-legacy"),
            serde_json::to_vec(&legacy_request).expect("encode legacy request"),
        )
        .expect("write legacy request");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        queue
            .start_pending_jobs_for_drain(&lock, &["fresh-legacy".to_owned()])
            .expect("start");
        drop(lock);
        let mut supervisor = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );

        supervisor.tick().expect("tick");

        assert_eq!(
            queue
                .get("fresh-legacy")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running,
            "legacy ownership must not be reclaimed before its heartbeat is stale"
        );
    }

    #[test]
    fn spawn_failure_requeues_without_claiming_execution() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "spawn-failure");
        let mut supervisor = ExecutionSupervisor::new(
            PathBuf::from("/does/not/exist"),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("tick");
        let job = Queue::new(temp.path())
            .expect("queue")
            .get("spawn-failure")
            .expect("read")
            .expect("job");
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.scheduler_defer_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn post_rename_receipt_error_retains_the_single_spawned_worker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let job = queued_job(temp.path(), "post-rename");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        let running = queue
            .start_pending_jobs_for_drain(&lock, std::slice::from_ref(&job.id))
            .expect("start")
            .remove(0);
        drop(lock);
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        fs::create_dir_all(supervisor.worker_dir()).expect("worker dir");

        supervisor
            .spawn_worker_with_receipt_writer(&running, |path, receipt| {
                write_json_atomic(path, receipt)?;
                Err(io::Error::other("injected parent-directory fsync failure"))
            })
            .expect("visible exact receipt is adopted");

        assert!(supervisor.children.contains_key(&job.id));
        assert!(supervisor.receipt_path(&job.id).exists());
        assert_eq!(
            queue.get(&job.id).expect("read").expect("job").status,
            JobStatus::Running
        );
        let mut child = supervisor.children.remove(&job.id).expect("worker");
        terminate_process_group(child.id());
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn unknown_worker_probe_preserves_running_and_blocks_admission() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "unknown-owner");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        queue
            .start_pending_jobs_for_drain(&lock, &["unknown-owner".to_owned()])
            .expect("start");
        drop(lock);
        let receipt = WorkerReceipt {
            job_id: "unknown-owner".to_owned(),
            generation: "generation".to_owned(),
            pid: u32::MAX,
            started_at: Utc::now(),
        };
        write_json_atomic(
            &temp.path().join("queue-workers/unknown-owner.json"),
            &receipt,
        )
        .expect("receipt");
        queued_job(temp.path(), "pending");
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );

        let unknown = supervisor
            .reconcile_running_with_probe(|_| ProcessLiveness::Unknown)
            .expect("reconcile");
        if !unknown {
            supervisor.admit_pending().expect("admit");
        }

        assert!(unknown);
        assert_eq!(
            queue
                .get("unknown-owner")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running
        );
        assert_eq!(
            queue.get("pending").expect("read").expect("job").status,
            JobStatus::Pending
        );
        assert!(supervisor.receipt_path("unknown-owner").exists());
    }

    #[cfg(unix)]
    #[test]
    fn scheduler_defer_deadline_prevents_immediate_worker_relaunch() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "deferred");
        let mut supervisor = ExecutionSupervisor::new(
            fake_worker(temp.path()),
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        );
        supervisor.tick().expect("start worker");
        let lease_store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        lease_store
            .acquire(&host_pool_lease_request("deferred"))
            .expect("acquire deferred lease")
            .expect("deferred lease");
        let mut queue = Queue::new(temp.path()).expect("queue");
        queue
            .requeue_deferred_daemon_worker(QueueDeferredRequeue {
                job_id: "deferred".to_owned(),
                reason: "capacity unavailable".to_owned(),
                defer_until: Some(Utc::now() + Duration::minutes(1)),
            })
            .expect("requeue")
            .expect("running job");

        supervisor.tick().expect("deferred tick");

        assert!(
            !supervisor.children.contains_key("deferred"),
            "the exact deferred worker tree must exit before Pending releases its claims"
        );
        assert_eq!(
            queue.get("deferred").expect("read").expect("job").status,
            JobStatus::Pending
        );
        assert!(
            lease_store.leases().expect("leases").is_empty(),
            "supervisor-owned deferred termination must not depend on worker Drop"
        );
    }
}
