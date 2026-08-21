//! Durable daemon ownership for queued execution workers.
//!
//! The supervisor deliberately never replays a job that reached `Running`.
//! A verified live worker may be adopted after daemon restart; otherwise a
//! stale running job becomes an explicit `UNCERTAIN` terminal outcome.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

use crate::identity::RuntimeMode;
use crate::job::{DEFAULT_RUNNING_JOB_STALE_SECONDS, Job, JobStatus};
use crate::queue::{Queue, QueueDeferredRequeue, QueueError, QueuePendingCancellation};
use crate::queue_request::{
    QueueRequestError, QueueRequestStore, QueuedExecutionEnvelope, QueuedExecutionOwner,
};
use crate::ship::persist_terminal_outcome;

const MAX_WORKERS: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkerObservation {
    Alive(WorkerReceipt),
    Dead,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessLiveness {
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

/// Errors surfaced by one supervisor tick.
#[derive(Debug)]
pub enum SupervisorError {
    /// Queue state failed.
    Queue(QueueError),
    /// Request-envelope state failed.
    Request(QueueRequestError),
    /// Worker process or receipt I/O failed.
    Io(io::Error),
    /// Terminal-outcome persistence failed.
    Outcome(String),
}

impl std::fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queue(error) => write!(formatter, "{error}"),
            Self::Request(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "execution supervisor I/O failed: {error}"),
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

/// Same-host daemon supervisor. Dropping it never signals workers.
pub struct ExecutionSupervisor {
    binary: PathBuf,
    mode: RuntimeMode,
    global_dir: PathBuf,
    state_dir: PathBuf,
    children: BTreeMap<String, Child>,
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
        }
    }

    /// Reconcile worker ownership and admit safe pending jobs.
    pub fn tick(&mut self) -> Result<(), SupervisorError> {
        fs::create_dir_all(self.worker_dir())?;
        self.terminate_cancelled_workers()?;
        self.terminate_deferred_workers()?;
        self.reap_owned_children()?;
        self.sweep_terminal_receipts()?;
        let unknown_worker = self.reconcile_running()?;
        if !unknown_worker {
            self.admit_pending()?;
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
            remove_if_present(&self.receipt_path(&job_id))?;
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
                remove_if_present(&self.receipt_path(&job.id))?;
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
            if let Some(mut child) = self.children.remove(job_id) {
                if terminate_child_tree(&mut child)? {
                    self.acknowledge_cancelled_job(&mut queue, job_id)?;
                    remove_if_present(&self.receipt_path(job_id))?;
                } else {
                    self.children.insert(job_id.clone(), child);
                }
                continue;
            }
            if let WorkerObservation::Alive(receipt) = self.observe_receipt(job_id)?
                && terminate_adopted_worker_tree(&receipt)
            {
                self.acknowledge_cancelled_job(&mut queue, job_id)?;
                remove_if_present(&self.receipt_path(job_id))?;
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
            if let Some(mut child) = self.children.remove(job_id) {
                if terminate_child_tree(&mut child)? {
                    remove_if_present(&self.receipt_path(job_id))?;
                } else {
                    self.children.insert(job_id.clone(), child);
                }
                continue;
            }
            if let WorkerObservation::Alive(receipt) = self.observe_receipt(job_id)? {
                terminate_process_group(receipt.pid);
                remove_if_present(&self.receipt_path(job_id))?;
            }
        }
        Ok(())
    }

    fn terminate_deferred_workers(&mut self) -> Result<(), SupervisorError> {
        let mut queue = Queue::new(&self.state_dir)?;
        let deferred = queue
            .get_running()?
            .into_iter()
            .filter(|job| job.cancel_requested_at.is_none() && job.scheduler_defer_reason.is_some())
            .map(|job| job.id)
            .collect::<Vec<_>>();
        for job_id in deferred {
            let tree_dead = if let Some(mut child) = self.children.remove(&job_id) {
                if terminate_child_tree(&mut child)? {
                    true
                } else {
                    self.children.insert(job_id.clone(), child);
                    false
                }
            } else if let WorkerObservation::Alive(receipt) = self.observe_receipt(&job_id)? {
                terminate_adopted_worker_tree(&receipt)
            } else {
                false
            };
            if tree_dead {
                let finalized = queue.finalize_deferred_daemon_worker(&job_id)?;
                if finalized.is_some() {
                    remove_if_present(&self.receipt_path(&job_id))?;
                } else if queue.get(&job_id)?.is_some_and(|job| {
                    job.status == JobStatus::Running && job.cancel_requested_at.is_some()
                }) {
                    self.acknowledge_cancelled_job(&mut queue, &job_id)?;
                    if queue
                        .get(&job_id)?
                        .is_none_or(|job| job.status != JobStatus::Running)
                    {
                        remove_if_present(&self.receipt_path(&job_id))?;
                    }
                }
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
            .cancel_with_reason(job.cancellation_reason.clone())
            .map_err(|error| SupervisorError::Outcome(error.to_string()))?;
        queue.update(&cancelled)?;
        persist_terminal_outcome(&cancelled, &self.state_dir)
            .map_err(|error| SupervisorError::Outcome(error.to_string()))
    }

    fn sweep_terminal_receipts(&self) -> Result<(), SupervisorError> {
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
            if self.observe_receipt(job_id)? == WorkerObservation::Dead {
                remove_if_present(&path)?;
            }
        }
        Ok(())
    }

    fn admit_pending(&mut self) -> Result<(), SupervisorError> {
        let mut queue = Queue::new(&self.state_dir)?;
        let Some(lock) = queue.acquire_drain_lock()? else {
            return Ok(());
        };
        let request_store = QueueRequestStore::new(&self.state_dir)?;
        let running = queue.get_running()?;
        let running_resources = running_resource_claims(&running, &request_store)?;
        let mut occupied = running_resources.claims;
        let live_count = running.len();
        if live_count >= MAX_WORKERS {
            return Ok(());
        }

        let pending = queue.get_pending()?;
        let mut selected = Vec::new();
        let mut cancellations = Vec::new();
        let now = Utc::now();
        for job in pending {
            if live_count + selected.len() >= MAX_WORKERS {
                break;
            }
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
                    });
                    continue;
                }
                Ok(None) => {
                    cancellations.push(QueuePendingCancellation {
                        job_id: job.id,
                        reason:
                            "queued execution request is missing; automatic execution is forbidden"
                                .to_owned(),
                    });
                    continue;
                }
                Err(error) if request_error_is_job_local(&error) => {
                    cancellations.push(QueuePendingCancellation {
                        job_id: job.id,
                        reason: format!(
                            "queued execution request is invalid; automatic execution is forbidden: {error}"
                        ),
                    });
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if !admissible(&envelope, &occupied) {
                continue;
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
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(&job.id))?;
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
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
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

    fn observe_receipt_with_probe(
        &self,
        job_id: &str,
        probe: impl FnOnce(&WorkerReceipt) -> ProcessLiveness,
    ) -> io::Result<WorkerObservation> {
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

fn terminate_adopted_worker_tree(receipt: &WorkerReceipt) -> bool {
    let Ok(descendants) = signal_process_tree(receipt.pid) else {
        return false;
    };
    let deadline = Instant::now() + StdDuration::from_secs(5);
    while process_liveness(receipt) == ProcessLiveness::Alive && Instant::now() < deadline {
        thread::sleep(StdDuration::from_millis(10));
    }
    if process_liveness(receipt) != ProcessLiveness::Dead {
        return false;
    }
    #[cfg(unix)]
    return descendants
        .iter()
        .all(|pid| process_id_liveness(*pid) == ProcessLiveness::Dead);
    #[cfg(windows)]
    true
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
        let script = format!(
            "$p=Get-CimInstance Win32_Process -Filter \"ProcessId={}\"; if($p){{$p.CommandLine}}",
            receipt.pid
        );
        let Ok(output) = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
        else {
            return ProcessLiveness::Unknown;
        };
        let command = String::from_utf8_lossy(&output.stdout);
        if output.status.success()
            && command.contains("execution-worker")
            && command.contains(&receipt.job_id)
            && command.contains(&receipt.generation)
        {
            ProcessLiveness::Alive
        } else if output.status.success() {
            ProcessLiveness::Dead
        } else {
            ProcessLiveness::Unknown
        }
    }
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
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobKind, Priority, ValidationMode};
    use crate::queue_request::{JobResourcePlan, QueueOutcomeStore};
    use std::sync::{LazyLock, Mutex};

    static PROCESS_TREE_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
    fn unrelated_claims_are_admitted_in_parallel() {
        let occupied = BTreeSet::from(["repo:a".to_owned()]);
        assert!(admissible(&envelope(&["repo:b"], true), &occupied));
    }

    #[test]
    fn conflicting_claims_and_legacy_requests_fail_closed() {
        let occupied = BTreeSet::from(["repo:a".to_owned()]);
        assert!(!admissible(&envelope(&["repo:a"], true), &occupied));
        assert!(!admissible(&envelope(&[], false), &BTreeSet::new()));
    }

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
        let deadline = Instant::now() + StdDuration::from_secs(15);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(10));
        }
        let descendant = fs::read_to_string(&pid_path).expect("descendant pid");
        let deadline = Instant::now() + StdDuration::from_secs(15);
        while !process_is_running(descendant.trim()) && Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(10));
        }
        assert!(process_is_running(descendant.trim()));

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
    fn restart_cancellation_verifies_adopted_worker_tree_before_reusing_capacity() {
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
        let deadline = Instant::now() + StdDuration::from_secs(15);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(10));
        }
        let descendant = fs::read_to_string(&pid_path).expect("descendant pid");
        let deadline = Instant::now() + StdDuration::from_secs(15);
        while !process_is_running(descendant.trim()) && Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(10));
        }
        assert!(process_is_running(descendant.trim()));
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
    }
}
