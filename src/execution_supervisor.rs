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

use crate::identity::RuntimeMode;
use crate::job::{Job, JobStatus};
use crate::queue::{Queue, QueueDeferredRequeue, QueueError, QueuePendingCancellation};
use crate::queue_request::{QueueRequestError, QueueRequestStore, QueuedExecutionEnvelope};
use crate::ship::persist_terminal_outcome;

const MAX_WORKERS: usize = 4;

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
        self.reap_owned_children()?;
        self.terminate_cancelled_workers()?;
        self.sweep_terminal_receipts()?;
        self.reconcile_running()?;
        self.admit_pending()?;
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
            self.children.remove(&job_id);
            let mut queue = Queue::new(&self.state_dir)?;
            let job = queue.get(&job_id)?;
            if matches!(job.as_ref().map(|job| job.status), Some(JobStatus::Running)) {
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
            remove_if_present(&self.receipt_path(&job_id))?;
        }
        Ok(())
    }

    fn reconcile_running(&mut self) -> Result<(), SupervisorError> {
        let mut queue = Queue::new(&self.state_dir)?;
        for job in queue.get_running()? {
            if self.children.contains_key(&job.id) {
                continue;
            }
            if self.load_live_receipt(&job.id)?.is_some() {
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
        Ok(())
    }

    fn terminate_cancelled_workers(&mut self) -> Result<(), SupervisorError> {
        let mut queue = Queue::new(&self.state_dir)?;
        let cancelled = queue
            .get_recent(usize::MAX)?
            .into_iter()
            .filter(|job| job.status == JobStatus::Cancelled)
            .map(|job| job.id)
            .collect::<BTreeSet<_>>();
        for job_id in &cancelled {
            if let Some(mut child) = self.children.remove(job_id) {
                terminate_process_group(child.id());
                let _ = child.wait();
                remove_if_present(&self.receipt_path(job_id))?;
                continue;
            }
            if let Some(receipt) = self.load_live_receipt(job_id)? {
                terminate_process_group(receipt.pid);
                remove_if_present(&self.receipt_path(job_id))?;
            }
        }
        Ok(())
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
            if self.load_live_receipt(job_id)?.is_none() {
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
        let mut occupied = running_resource_claims(&running, &request_store)?;
        let live_count = running.len();
        if live_count >= MAX_WORKERS {
            return Ok(());
        }

        let pending = queue.get_pending()?;
        let mut selected = Vec::new();
        let mut cancellations = Vec::new();
        for job in pending {
            if live_count + selected.len() >= MAX_WORKERS {
                break;
            }
            let envelope = match request_store.load(&job.id) {
                Ok(Some(envelope)) if envelope.provenance.is_some() => envelope,
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
        let started = queue.start_pending_jobs_for_drain(&lock, &selected)?;
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
        let child = command.spawn()?;
        let receipt = WorkerReceipt {
            job_id: job.id.clone(),
            generation,
            pid: child.id(),
            started_at: Utc::now(),
        };
        write_json_atomic(&self.receipt_path(&job.id), &receipt)?;
        self.children.insert(job.id.clone(), child);
        Ok(())
    }

    fn load_live_receipt(&self, job_id: &str) -> io::Result<Option<WorkerReceipt>> {
        let path = self.receipt_path(job_id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let receipt: WorkerReceipt = if let Ok(receipt) = serde_json::from_slice(&bytes) {
            receipt
        } else {
            remove_if_present(&path)?;
            return Ok(None);
        };
        if receipt.job_id != job_id || !worker_identity_is_live(&receipt) {
            remove_if_present(&path)?;
            return Ok(None);
        }
        Ok(Some(receipt))
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

fn running_resource_claims(
    running: &[Job],
    store: &QueueRequestStore,
) -> Result<BTreeSet<String>, QueueRequestError> {
    let mut claims = BTreeSet::new();
    for job in running {
        match store.load(&job.id) {
            Ok(Some(envelope)) => claims.extend(resource_claims(&envelope)),
            Ok(None) => {}
            Err(error) if request_error_is_job_local(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(claims)
}

fn request_error_is_job_local(error: &QueueRequestError) -> bool {
    matches!(
        error,
        QueueRequestError::Json(_)
            | QueueRequestError::UnsupportedSchema { .. }
            | QueueRequestError::InvalidSnapshot { .. }
    )
}

fn terminate_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    let _ = pid;
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

fn worker_identity_is_live(receipt: &WorkerReceipt) -> bool {
    let output = Command::new("/bin/ps")
        .args(["-p", &receipt.pid.to_string(), "-o", "command="])
        .output();
    let Ok(output) = output else {
        return false;
    };
    let command = String::from_utf8_lossy(&output.stdout);
    output.status.success()
        && command.contains("execution-worker")
        && command.contains(&receipt.job_id)
        && command.contains(&receipt.generation)
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
        assert!(!worker_identity_is_live(&receipt));
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
                "#!/bin/sh\n/bin/sleep 30 & echo $! > '{}'\nwait\n",
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
        Command::new("/bin/kill")
            .args(["-0", "--", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
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
        let deadline = Instant::now() + StdDuration::from_secs(5);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(10));
        }
        let descendant = fs::read_to_string(&pid_path).expect("descendant pid");
        assert!(process_is_running(descendant.trim()));

        let mut queue = Queue::new(temp.path()).expect("queue");
        let cancelled = queue
            .get("cancel-tree")
            .expect("read")
            .expect("job")
            .cancel_with_reason(Some("operator cancel".to_owned()))
            .expect("cancel");
        queue.update(&cancelled).expect("persist cancel");
        supervisor.tick().expect("terminate cancelled worker");

        let deadline = Instant::now() + StdDuration::from_secs(5);
        while process_is_running(descendant.trim()) && Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(10));
        }
        assert!(!process_is_running(descendant.trim()));
        assert!(supervisor.children.is_empty());
        assert!(!supervisor.receipt_path("cancel-tree").exists());
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

    #[test]
    fn stale_running_without_worker_becomes_uncertain_and_durable() {
        let temp = tempfile::tempdir().expect("tempdir");
        queued_job(temp.path(), "lost-worker");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        let running = queue
            .start_pending_jobs_for_drain(&lock, &["lost-worker".to_owned()])
            .expect("start")
            .remove(0);
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
}
