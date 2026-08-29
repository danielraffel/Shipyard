//! Closed protocol between durable canary custody and Shipyard's daemon-owned
//! process supervisor.
//!
//! The CLI may submit immutable work, but only a daemon advertising the exact
//! capability below may instantiate this backend. There is deliberately no
//! foreground, shell, `nohup`, or ambient-cwd fallback.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::identity::RuntimeMode;
use crate::immutable_store::{ImmutableByteStore, ImmutableStoreError};
use crate::parallel_proof::Sha256Digest;
use crate::parallel_proof_canary_job::{
    ApprovedCanaryJob, ApprovedCanaryOperation, CanaryCancellationObservation, CanaryJobBackend,
    CanaryProcessObservation, CanaryProcessTreeIdentity,
};

/// Exact daemon status capability required before production submission.
pub const DAEMON_CANARY_JOB_CAPABILITY: &str = "parallel_proof_canary_job_v1";
const MAX_WORKER_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SUPERVISOR_RECORD_BYTES: usize = 64 * 1024;
const MAX_DAEMON_CANARY_TICK_INTERVAL_MS: u64 = 1_000;
const MAX_DAEMON_CANARY_JOBS_PER_TICK: usize = 1;

pub(crate) fn executable_digest(path: &Path) -> Result<Sha256Digest, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_WORKER_BINARY_BYTES {
        return Err("canary worker binary is not a bounded regular file".to_owned());
    }
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| "canary worker binary size exceeds this platform".to_owned())?;
    let mut bytes = Vec::with_capacity(capacity);
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 != metadata.len() {
        return Err("canary worker binary changed while hashing".to_owned());
    }
    Ok(Sha256Digest::of_bytes(&bytes))
}

/// Return true only when the live daemon explicitly advertises the typed lane.
#[must_use]
pub fn daemon_supports_canary_jobs(status: &serde_json::Value) -> bool {
    status
        .get("capabilities")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|capabilities| {
            capabilities
                .iter()
                .any(|capability| capability.as_str() == Some(DAEMON_CANARY_JOB_CAPABILITY))
        })
}

/// Closed launch request passed from custody to the daemon-owned supervisor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanarySupervisedLaunch {
    /// Exact immutable job envelope.
    pub job: ApprovedCanaryJob,
    /// Digest of the exact envelope, repeated for protocol framing.
    pub job_sha256: Sha256Digest,
    /// Prepared-receipt nonce; the worker must publish it in its identity.
    pub launch_nonce_sha256: Sha256Digest,
    /// Controller time at the exclusive immutable launch claim.
    pub claimed_at_ms: u64,
}

impl CanarySupervisedLaunch {
    fn validate(&self) -> Result<(), String> {
        self.job.validate().map_err(|error| error.to_string())?;
        if self.job.digest().map_err(|error| error.to_string())? != self.job_sha256 {
            return Err("supervised canary launch job digest mismatch".to_owned());
        }
        Ok(())
    }
}

/// Daemon-owned process mechanics. Implementations choose the already-pinned
/// Shipyard worker binary; the request contains no executable path or arguments.
pub trait CanaryProcessSupervisor {
    /// Spawn the hidden typed worker in the daemon's supervised process group.
    fn launch_typed_worker(
        &mut self,
        request: &CanarySupervisedLaunch,
    ) -> Result<CanaryProcessTreeIdentity, String>;

    /// Discover an original launch by immutable nonce after daemon restart.
    fn discover_typed_worker(
        &mut self,
        job: &ApprovedCanaryJob,
        launch_nonce_sha256: &Sha256Digest,
    ) -> Result<CanaryProcessObservation, String>;

    /// Observe the exact process generation without redispatch.
    fn observe_typed_worker(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
    ) -> Result<CanaryProcessObservation, String>;

    /// Terminate and prove the complete supervised tree within the grace bound.
    fn cancel_typed_worker(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
        grace_ms: u64,
    ) -> Result<CanaryCancellationObservation, String>;
}

/// Production adapter used by the daemon lane to drive durable custody.
pub struct DaemonCanaryJobBackend<S> {
    supervisor: S,
}

impl<S> DaemonCanaryJobBackend<S> {
    /// Bind one daemon-owned supervisor. Constructing this does not launch work.
    pub const fn new(supervisor: S) -> Self {
        Self { supervisor }
    }
}

impl<S: CanaryProcessSupervisor> CanaryJobBackend for DaemonCanaryJobBackend<S> {
    fn launch(
        &mut self,
        job: &ApprovedCanaryJob,
        launch_nonce_sha256: &Sha256Digest,
        claimed_at_ms: u64,
    ) -> Result<CanaryProcessTreeIdentity, String> {
        let request = CanarySupervisedLaunch {
            job: job.clone(),
            job_sha256: job.digest().map_err(|error| error.to_string())?,
            launch_nonce_sha256: launch_nonce_sha256.clone(),
            claimed_at_ms,
        };
        request.validate()?;
        self.supervisor.launch_typed_worker(&request)
    }

    fn discover(
        &mut self,
        job: &ApprovedCanaryJob,
        launch_nonce_sha256: &Sha256Digest,
    ) -> Result<CanaryProcessObservation, String> {
        self.supervisor
            .discover_typed_worker(job, launch_nonce_sha256)
    }

    fn observe(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
    ) -> Result<CanaryProcessObservation, String> {
        self.supervisor.observe_typed_worker(job, process)
    }

    fn cancel(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
        grace_ms: u64,
    ) -> Result<CanaryCancellationObservation, String> {
        self.supervisor.cancel_typed_worker(job, process, grace_ms)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CanarySupervisorReceipt {
    job_id: String,
    generation: String,
    process: CanaryProcessTreeIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CanaryWorkerCompletion {
    job_id: String,
    generation: String,
    exit_code: i32,
    completed_at_ms: u64,
    artifact: Option<crate::parallel_proof_canary_job::CanaryJobArtifact>,
}

#[derive(Clone, Debug)]
struct CanarySupervisorStore {
    records: ImmutableByteStore,
}

impl CanarySupervisorStore {
    fn open(state_dir: &Path) -> Result<Self, String> {
        let parent = state_dir.join("parallel-proof-canary");
        crate::writer_domain_lease::ensure_protected_dir_all(&parent)
            .map_err(|error| error.to_string())?;
        ImmutableByteStore::open(parent.join("supervisor"), MAX_SUPERVISOR_RECORD_BYTES)
            .map(|records| Self { records })
            .map_err(|error| store_error(&error))
    }

    fn put_receipt(&self, receipt: &CanarySupervisorReceipt) -> Result<(), String> {
        self.records
            .put(
                &format!("{}-receipt", receipt.job_id),
                &serde_json::to_vec(receipt).map_err(|error| error.to_string())?,
            )
            .map(|_| ())
            .map_err(|error| store_error(&error))
    }

    fn receipt(&self, job_id: &str) -> Result<Option<CanarySupervisorReceipt>, String> {
        load_optional(&self.records, &format!("{job_id}-receipt"))
    }

    fn put_completion(&self, completion: &CanaryWorkerCompletion) -> Result<(), String> {
        self.records
            .put(
                &format!("{}-completion", completion.job_id),
                &serde_json::to_vec(completion).map_err(|error| error.to_string())?,
            )
            .map(|_| ())
            .map_err(|error| store_error(&error))
    }

    fn completion(&self, job_id: &str) -> Result<Option<CanaryWorkerCompletion>, String> {
        load_optional(&self.records, &format!("{job_id}-completion"))
    }
}

fn load_optional<T: for<'de> Deserialize<'de>>(
    store: &ImmutableByteStore,
    key: &str,
) -> Result<Option<T>, String> {
    match store.load(key) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(ImmutableStoreError::Missing(_)) => Ok(None),
        Err(error) => Err(store_error(&error)),
    }
}

fn store_error(error: &ImmutableStoreError) -> String {
    error.to_string()
}

/// Daemon-owned implementation of the closed worker protocol.
pub(crate) struct ShipyardCanaryProcessSupervisor {
    binary: PathBuf,
    binary_sha256: Sha256Digest,
    mode: RuntimeMode,
    global_dir: PathBuf,
    state_dir: PathBuf,
    store: CanarySupervisorStore,
    children: BTreeMap<String, Child>,
}

impl ShipyardCanaryProcessSupervisor {
    pub(crate) fn new(
        binary: PathBuf,
        mode: RuntimeMode,
        global_dir: PathBuf,
        state_dir: PathBuf,
    ) -> Result<Self, String> {
        let binary_sha256 = executable_digest(&binary)?;
        let store = CanarySupervisorStore::open(&state_dir)?;
        Ok(Self {
            binary,
            binary_sha256,
            mode,
            global_dir,
            state_dir,
            store,
            children: BTreeMap::new(),
        })
    }

    fn receipt_for(
        &self,
        job: &ApprovedCanaryJob,
        launch_nonce_sha256: &Sha256Digest,
    ) -> Result<Option<CanarySupervisorReceipt>, String> {
        let Some(receipt) = self.store.receipt(&job.job_id)? else {
            return Ok(None);
        };
        let ApprovedCanaryOperation::ParallelProofDistributedShadow {
            worker_executable_sha256,
            ..
        } = &job.operation;
        if receipt.job_id != job.job_id
            || receipt.generation != launch_nonce_sha256.as_str()
            || receipt.process.launch_nonce_sha256 != *launch_nonce_sha256
            || receipt.process.executable_sha256 != *worker_executable_sha256
        {
            return Err("canary supervisor receipt authority mismatch".to_owned());
        }
        Ok(Some(receipt))
    }

    fn observation(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
    ) -> Result<CanaryProcessObservation, String> {
        let receipt = self
            .receipt_for(job, &process.launch_nonce_sha256)?
            .ok_or_else(|| "canary supervisor receipt is missing".to_owned())?;
        if receipt.process != *process {
            return Ok(CanaryProcessObservation::IdentityMismatch);
        }
        let completion = self.store.completion(&job.job_id)?;
        if let Some(child) = self.children.get_mut(&job.job_id) {
            match child.try_wait().map_err(|error| error.to_string())? {
                None => return Ok(CanaryProcessObservation::Alive(process.clone())),
                Some(status) => {
                    let completion = completion.unwrap_or(CanaryWorkerCompletion {
                        job_id: job.job_id.clone(),
                        generation: receipt.generation.clone(),
                        exit_code: status.code().unwrap_or(1),
                        completed_at_ms: controller_now_ms()?,
                        artifact: None,
                    });
                    self.store.put_completion(&completion)?;
                    self.children.remove(&job.job_id);
                    return Ok(CanaryProcessObservation::Exited {
                        process: process.clone(),
                        exit_code: Some(completion.exit_code),
                        exited_at_ms: completion.completed_at_ms,
                        artifact: completion.artifact,
                    });
                }
            }
        }
        if let Some(completion) = completion {
            if completion.job_id != job.job_id
                || completion.generation != receipt.generation
                || completion.completed_at_ms < process.launched_at_ms
            {
                return Ok(CanaryProcessObservation::IdentityMismatch);
            }
            return match worker_process_liveness(&receipt) {
                WorkerLiveness::Alive => Ok(CanaryProcessObservation::Alive(process.clone())),
                WorkerLiveness::Dead => Ok(CanaryProcessObservation::Exited {
                    process: process.clone(),
                    exit_code: Some(completion.exit_code),
                    exited_at_ms: completion.completed_at_ms,
                    artifact: completion.artifact,
                }),
                WorkerLiveness::IdentityMismatch => Ok(CanaryProcessObservation::IdentityMismatch),
                WorkerLiveness::Unknown => Err("canary worker liveness is unknown".to_owned()),
            };
        }
        match worker_process_liveness(&receipt) {
            WorkerLiveness::Alive => Ok(CanaryProcessObservation::Alive(process.clone())),
            WorkerLiveness::Dead => Ok(CanaryProcessObservation::Missing),
            WorkerLiveness::IdentityMismatch => Ok(CanaryProcessObservation::IdentityMismatch),
            WorkerLiveness::Unknown => Err("canary worker liveness is unknown".to_owned()),
        }
    }
}

impl CanaryProcessSupervisor for ShipyardCanaryProcessSupervisor {
    fn launch_typed_worker(
        &mut self,
        request: &CanarySupervisedLaunch,
    ) -> Result<CanaryProcessTreeIdentity, String> {
        request.validate()?;
        let ApprovedCanaryOperation::ParallelProofDistributedShadow {
            worker_executable_sha256,
            ..
        } = &request.job.operation;
        if *worker_executable_sha256 != self.binary_sha256 {
            return Err("canary worker binary digest mismatch".to_owned());
        }
        if let Some(existing) = self.receipt_for(&request.job, &request.launch_nonce_sha256)? {
            return Ok(existing.process);
        }
        let log_dir = self.state_dir.join("parallel-proof-canary").join("logs");
        crate::writer_domain_lease::ensure_protected_dir_all(&log_dir)
            .map_err(|error| error.to_string())?;
        let log_path = log_dir.join(format!("{}.log", request.job.job_id));
        let retention_policy =
            crate::config::LoadedConfig::load_machine_global_from_dir(self.global_dir.clone())
                .map_or_else(
                    |_| crate::log_retention::LogRetentionPolicy::default(),
                    |config| crate::log_retention::LogRetentionPolicy::from_config(&config),
                );
        crate::log_retention::rotate_if_oversize(&log_path, retention_policy)
            .map_err(|error| error.to_string())?;
        let writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&log_path)
            .map_err(|error| error.to_string())?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| error.to_string())?;
        drop(writer_domain);
        let stderr = log.try_clone().map_err(|error| error.to_string())?;
        let mut command = Command::new(&self.binary);
        command
            .arg("--mode")
            .arg(self.mode.as_str())
            .arg("--global-dir")
            .arg(&self.global_dir)
            .arg("--state-dir")
            .arg(&self.state_dir)
            .arg("parallel-proof-canary-worker")
            .arg("--job-id")
            .arg(&request.job.job_id)
            .arg("--generation")
            .arg(request.launch_nonce_sha256.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let pid = child.id();
        let os_start_identity_sha256 = match os_process_start_identity(pid) {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                let _ = terminate_process_group(pid, Duration::from_secs(1));
                let _ = child.wait();
                return Err("canary worker exited before start identity was captured".to_owned());
            }
            Err(error) => {
                let _ = terminate_process_group(pid, Duration::from_secs(1));
                let _ = child.wait();
                return Err(error);
            }
        };
        let process = CanaryProcessTreeIdentity {
            pid,
            tree_id: format!("pgrp-{pid}"),
            birth_token: request.launch_nonce_sha256.as_str().to_owned(),
            os_start_identity_sha256,
            launch_nonce_sha256: request.launch_nonce_sha256.clone(),
            executable_sha256: self.binary_sha256.clone(),
            launched_at_ms: request.claimed_at_ms,
        };
        let receipt = CanarySupervisorReceipt {
            job_id: request.job.job_id.clone(),
            generation: request.launch_nonce_sha256.as_str().to_owned(),
            process: process.clone(),
        };
        if let Err(error) = self.store.put_receipt(&receipt) {
            let _ = terminate_process_group(pid, Duration::from_secs(1));
            let _ = child.wait();
            return Err(error);
        }
        self.children.insert(request.job.job_id.clone(), child);
        Ok(process)
    }

    fn discover_typed_worker(
        &mut self,
        job: &ApprovedCanaryJob,
        launch_nonce_sha256: &Sha256Digest,
    ) -> Result<CanaryProcessObservation, String> {
        let Some(receipt) = self.receipt_for(job, launch_nonce_sha256)? else {
            return Ok(CanaryProcessObservation::Missing);
        };
        self.observation(job, &receipt.process)
    }

    fn observe_typed_worker(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
    ) -> Result<CanaryProcessObservation, String> {
        self.observation(job, process)
    }

    fn cancel_typed_worker(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
        grace_ms: u64,
    ) -> Result<CanaryCancellationObservation, String> {
        let Some(receipt) = self.receipt_for(job, &process.launch_nonce_sha256)? else {
            return Ok(CanaryCancellationObservation::Missing);
        };
        if receipt.process != *process {
            return Ok(CanaryCancellationObservation::Missing);
        }
        match worker_process_liveness(&receipt) {
            WorkerLiveness::Alive => {}
            WorkerLiveness::Dead | WorkerLiveness::IdentityMismatch => {
                return Ok(CanaryCancellationObservation::Missing);
            }
            WorkerLiveness::Unknown => {
                return Err(
                    "canary worker identity cannot be revalidated for cancellation".to_owned(),
                );
            }
        }
        if terminate_process_group(process.pid, Duration::from_millis(grace_ms))? {
            if let Some(mut child) = self.children.remove(&job.job_id) {
                let _ = child.wait();
            }
            Ok(CanaryCancellationObservation::Terminated)
        } else {
            Ok(CanaryCancellationObservation::StillAlive)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerLiveness {
    Alive,
    Dead,
    IdentityMismatch,
    Unknown,
}

fn worker_process_liveness(receipt: &CanarySupervisorReceipt) -> WorkerLiveness {
    #[cfg(unix)]
    {
        match os_process_start_identity(receipt.process.pid) {
            Ok(Some(identity)) if identity == receipt.process.os_start_identity_sha256 => {}
            Ok(Some(_)) => return WorkerLiveness::IdentityMismatch,
            Ok(None) => return WorkerLiveness::Dead,
            Err(_) => return WorkerLiveness::Unknown,
        }
        let output = Command::new("/bin/ps")
            .args([
                "-ww",
                "-p",
                &receipt.process.pid.to_string(),
                "-o",
                "command=",
            ])
            .output();
        let Ok(output) = output else {
            return WorkerLiveness::Unknown;
        };
        let command = String::from_utf8_lossy(&output.stdout);
        if output.status.success()
            && command.contains("parallel-proof-canary-worker")
            && command.contains(&receipt.job_id)
            && command.contains(&receipt.generation)
        {
            WorkerLiveness::Alive
        } else if !output.status.success() && !output.stderr.is_empty() {
            WorkerLiveness::Unknown
        } else {
            WorkerLiveness::Dead
        }
    }
    #[cfg(not(unix))]
    {
        let _ = receipt;
        WorkerLiveness::Unknown
    }
}

#[cfg(unix)]
fn os_process_start_identity(pid: u32) -> Result<Option<Sha256Digest>, String> {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && !value.is_empty() {
        Ok(Some(Sha256Digest::of_bytes(value.as_bytes())))
    } else if !output.status.success() && output.stderr.is_empty() {
        Ok(None)
    } else {
        Err("canary worker OS start identity is unavailable".to_owned())
    }
}

#[cfg(unix)]
fn terminate_process_group(pid: u32, grace: Duration) -> Result<bool, String> {
    let _ = grace;
    crate::execution_supervisor::terminate_detached_worker_tree(pid)
        .map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn terminate_process_group(_pid: u32, _grace: Duration) -> Result<bool, String> {
    Ok(false)
}

fn controller_now_ms() -> Result<u64, String> {
    u64::try_from(Utc::now().timestamp_millis()).map_err(|_| "controller time overflow".to_owned())
}

pub(crate) fn verify_worker_authority(
    state_dir: &Path,
    job_id: &str,
    generation: &str,
) -> Result<CanaryProcessTreeIdentity, String> {
    let store = CanarySupervisorStore::open(state_dir)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(receipt) = store.receipt(job_id)? {
            if receipt.job_id == job_id
                && receipt.generation == generation
                && receipt.process.pid == std::process::id()
                && receipt.process.birth_token == generation
                && os_process_start_identity(std::process::id())?
                    == Some(receipt.process.os_start_identity_sha256.clone())
            {
                return Ok(receipt.process);
            }
            return Err("canary worker receipt authority mismatch".to_owned());
        }
        if Instant::now() >= deadline {
            return Err("canary worker receipt authority is missing".to_owned());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(crate) fn record_worker_completion(
    state_dir: &Path,
    job_id: &str,
    generation: &str,
    exit_code: i32,
    artifact: Option<crate::parallel_proof_canary_job::CanaryJobArtifact>,
) -> Result<(), String> {
    CanarySupervisorStore::open(state_dir)?.put_completion(&CanaryWorkerCompletion {
        job_id: job_id.to_owned(),
        generation: generation.to_owned(),
        exit_code,
        completed_at_ms: controller_now_ms()?,
        artifact,
    })
}

/// One default-off daemon lane for pending/restartable canary custody.
pub(crate) struct DaemonCanaryJobRuntime {
    store: crate::parallel_proof_canary_job::CanaryJobStore,
    backend: DaemonCanaryJobBackend<ShipyardCanaryProcessSupervisor>,
    state_dir: PathBuf,
    next_tick_at_ms: u64,
    next_job_after: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DaemonCanaryTickReport {
    pub(crate) ran: bool,
    pub(crate) processed_jobs: usize,
    pub(crate) warning: Option<String>,
}

impl DaemonCanaryJobRuntime {
    /// Construct only when trusted machine-global activation is complete.
    pub(crate) fn from_config(
        binary: PathBuf,
        mode: RuntimeMode,
        global_dir: PathBuf,
        state_dir: &Path,
        config: &crate::config::LoadedConfig,
    ) -> Result<Option<Self>, String> {
        if crate::parallel_proof_canary_adapter::trusted_parallel_proof_canary_config(config)
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Ok(None);
        }
        let supervisor = ShipyardCanaryProcessSupervisor::new(
            binary,
            mode,
            global_dir,
            state_dir.to_path_buf(),
        )?;
        let store = crate::parallel_proof_canary_job::CanaryJobStore::open(
            state_dir.join("parallel-proof-canary").join("jobs"),
        )
        .map_err(|error| error.to_string())?;
        Ok(Some(Self {
            store,
            backend: DaemonCanaryJobBackend::new(supervisor),
            state_dir: state_dir.to_path_buf(),
            next_tick_at_ms: 0,
            next_job_after: None,
        }))
    }

    pub(crate) fn tick(&mut self, now_ms: u64) -> Result<DaemonCanaryTickReport, String> {
        if now_ms < self.next_tick_at_ms {
            return Ok(DaemonCanaryTickReport::default());
        }
        self.next_tick_at_ms = now_ms.saturating_add(MAX_DAEMON_CANARY_TICK_INTERVAL_MS);
        let mut first_error = None;
        let mut next_interval_ms = MAX_DAEMON_CANARY_TICK_INTERVAL_MS;
        let (pending_job_ids, scan_errors) = self
            .store
            .pending_job_scan()
            .map_err(|error| error.to_string())?;
        for error in scan_errors {
            first_error.get_or_insert(error);
        }
        let selected_job_ids = bounded_job_batch(
            &pending_job_ids,
            self.next_job_after.as_deref(),
            MAX_DAEMON_CANARY_JOBS_PER_TICK,
        );
        let processed_jobs = selected_job_ids.len();
        for job_id in selected_job_ids {
            self.next_job_after = Some(job_id.clone());
            let snapshot = match self.store.load(&job_id) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    first_error.get_or_insert_with(|| error.to_string());
                    continue;
                }
            };
            next_interval_ms = next_interval_ms
                .min(snapshot.job.heartbeat_interval_ms)
                .min((snapshot.job.heartbeat_timeout_ms / 2).max(1));
            let result = if matches!(
                snapshot.latest().receipt,
                crate::parallel_proof_canary_job::CanaryJobReceiptState::Prepared { .. }
            ) {
                if now_ms >= snapshot.job.deadline_at_ms {
                    snapshot
                        .job
                        .digest()
                        .map_err(|error| error.to_string())
                        .and_then(|job_sha256| {
                            self.store
                                .request_cancel(
                                    &job_id,
                                    &crate::parallel_proof_canary_job::CanaryCancellationRequest {
                                        job_sha256,
                                        controller_id: snapshot.job.owner.controller_id.clone(),
                                        approval_sha256: snapshot.job.owner.approval_sha256.clone(),
                                        requested_at_ms: now_ms,
                                    },
                                )
                                .map(|_| None)
                                .map_err(|error| error.to_string())
                        })
                } else {
                    crate::parallel_proof_canary_job::launch_canary_job(
                        &self.store,
                        &snapshot.job,
                        now_ms,
                        &mut self.backend,
                    )
                    .map(Some)
                    .map_err(|error| error.to_string())
                }
            } else {
                crate::parallel_proof_canary_job::reconcile_canary_job(
                    &self.store,
                    &job_id,
                    now_ms,
                    &mut self.backend,
                )
                .map(Some)
                .map_err(|error| error.to_string())
            };
            match result {
                Ok(Some(transition)) if transition.wake => {
                    if let Err(error) = self.deliver_terminal_wake(&transition, now_ms) {
                        first_error.get_or_insert(error);
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        self.next_tick_at_ms = now_ms.saturating_add(next_interval_ms);
        Ok(DaemonCanaryTickReport {
            ran: true,
            processed_jobs,
            warning: first_error,
        })
    }

    fn deliver_terminal_wake(
        &self,
        transition: &crate::parallel_proof_canary_job::CanaryJobTransition,
        now_ms: u64,
    ) -> Result<(), String> {
        let binding = transition
            .snapshot
            .job
            .native_continuation
            .as_ref()
            .ok_or_else(|| {
                "terminal canary wake lacks admitted native continuation authority".to_owned()
            })?;
        let ledger = crate::work_ledger::WorkLedger::open_existing(&self.state_dir)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "native work ledger is unavailable for canary wake".to_owned())?;
        let job_sha256 = transition
            .snapshot
            .job
            .digest()
            .map_err(|error| error.to_string())?;
        let terminal_receipt_sha256 = transition
            .snapshot
            .latest()
            .digest()
            .map_err(|error| error.to_string())?;
        let delivery = ledger
            .deliver_canary_terminal_wake(binding, &job_sha256, &terminal_receipt_sha256)
            .map_err(|error| error.to_string())?;
        self.store
            .acknowledge_wake(
                &transition.snapshot.job.job_id,
                &crate::parallel_proof_canary_job::CanaryWakeAcknowledgement {
                    job_sha256,
                    receipt_sha256: terminal_receipt_sha256,
                    controller_id: transition.snapshot.job.owner.controller_id.clone(),
                    approval_sha256: transition.snapshot.job.owner.approval_sha256.clone(),
                    native_wake_id: Some(delivery.wake_id),
                    native_delivery_sha256: Some(delivery.receipt_sha256),
                    acknowledged_at_ms: now_ms,
                },
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn bounded_job_batch(job_ids: &[String], after: Option<&str>, limit: usize) -> Vec<String> {
    if job_ids.is_empty() || limit == 0 {
        return Vec::new();
    }
    let start = after.map_or(0, |cursor| {
        job_ids.partition_point(|job_id| job_id.as_str() <= cursor) % job_ids.len()
    });
    (0..job_ids.len().min(limit))
        .map(|offset| job_ids[(start + offset) % job_ids.len()].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command};

    use super::*;
    use crate::parallel_proof_canary_job::{
        ApprovedCanaryOperation, CanaryCancellationPolicy, CanaryJobOwner, CanaryLogPolicy,
        CanarySuccessPredicate, CanaryWakePredicate, launch_canary_job,
    };

    fn digest(value: &str) -> Sha256Digest {
        Sha256Digest::of_bytes(value.as_bytes())
    }

    fn job(executable_sha256: Sha256Digest) -> ApprovedCanaryJob {
        ApprovedCanaryJob {
            schema_version: 1,
            job_id: "adapter-real-child".to_owned(),
            correlation_id: "adapter-real-child".to_owned(),
            owner: CanaryJobOwner {
                controller_id: "controller".to_owned(),
                controller_incarnation: "incarnation".to_owned(),
                approval_sha256: digest("approval"),
            },
            operation: ApprovedCanaryOperation::ParallelProofDistributedShadow {
                repository_id: 42,
                repository: "Generous-Corp/pulp".to_owned(),
                target: "macos".to_owned(),
                target_triple: "aarch64-apple-darwin".to_owned(),
                builder_host_id: "m3".to_owned(),
                worker_host_id: "m1".to_owned(),
                manifest_sha256: digest("manifest"),
                request_sha256: digest("request"),
                release_sha256: digest("release"),
                builder_session_generation: 3,
                worker_session_generation: 5,
                cache_authority_sha256: digest("cache"),
                storage_authority_sha256: digest("storage"),
                artifact_bytes_total: 1024,
                invocation_authority_sha256: digest("invocation"),
                adapter_executable_sha256: digest("pinned-adapter"),
                worker_executable_sha256: executable_sha256,
            },
            approved_at_ms: 1,
            deadline_at_ms: 60_000,
            heartbeat_interval_ms: 100,
            heartbeat_timeout_ms: 1_000,
            max_heartbeat_receipts: 4,
            success: CanarySuccessPredicate {
                required_exit_code: 0,
                artifact_schema_version: 1,
                max_artifact_bytes: 4096,
            },
            cancellation: CanaryCancellationPolicy {
                grace_ms: 1_000,
                cancel_at_deadline: true,
            },
            wake: CanaryWakePredicate {
                on_success: true,
                on_actionable_failure: true,
            },
            native_continuation: None,
            logs: CanaryLogPolicy {
                segment_bytes: 1024,
                max_segments: 2,
            },
        }
    }

    #[cfg(unix)]
    struct RealChildSupervisor {
        child: Option<Child>,
        executable_sha256: Sha256Digest,
    }

    #[cfg(unix)]
    impl CanaryProcessSupervisor for RealChildSupervisor {
        fn launch_typed_worker(
            &mut self,
            request: &CanarySupervisedLaunch,
        ) -> Result<CanaryProcessTreeIdentity, String> {
            use std::os::unix::process::CommandExt as _;
            let mut command = Command::new("/bin/sleep");
            command.arg("30").process_group(0);
            let child = command.spawn().map_err(|error| error.to_string())?;
            let pid = child.id();
            self.child = Some(child);
            Ok(CanaryProcessTreeIdentity {
                pid,
                tree_id: format!("pgrp-{pid}"),
                birth_token: format!("test-{pid}"),
                os_start_identity_sha256: digest("test-start"),
                launch_nonce_sha256: request.launch_nonce_sha256.clone(),
                executable_sha256: self.executable_sha256.clone(),
                launched_at_ms: request.claimed_at_ms,
            })
        }

        fn discover_typed_worker(
            &mut self,
            _job: &ApprovedCanaryJob,
            _launch_nonce_sha256: &Sha256Digest,
        ) -> Result<CanaryProcessObservation, String> {
            Ok(CanaryProcessObservation::Missing)
        }

        fn observe_typed_worker(
            &mut self,
            _job: &ApprovedCanaryJob,
            process: &CanaryProcessTreeIdentity,
        ) -> Result<CanaryProcessObservation, String> {
            match self.child.as_mut().ok_or("missing child")?.try_wait() {
                Ok(None) => Ok(CanaryProcessObservation::Alive(process.clone())),
                Ok(Some(status)) => Ok(CanaryProcessObservation::Exited {
                    process: process.clone(),
                    exit_code: status.code(),
                    exited_at_ms: process.launched_at_ms + 1,
                    artifact: None,
                }),
                Err(error) => Err(error.to_string()),
            }
        }

        fn cancel_typed_worker(
            &mut self,
            _job: &ApprovedCanaryJob,
            _process: &CanaryProcessTreeIdentity,
            _grace_ms: u64,
        ) -> Result<CanaryCancellationObservation, String> {
            let child = self.child.as_mut().ok_or("missing child")?;
            child.kill().map_err(|error| error.to_string())?;
            child.wait().map_err(|error| error.to_string())?;
            Ok(CanaryCancellationObservation::Terminated)
        }
    }

    #[cfg(unix)]
    #[test]
    fn typed_boundary_launches_a_real_child_without_command_authority() {
        let _guard = crate::test_support::PROCESS_TREE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let executable_sha256 = digest("pinned-shipyard-worker");
        let job = job(executable_sha256.clone());
        let temp = tempfile::tempdir().unwrap();
        let store = crate::parallel_proof_canary_job::CanaryJobStore::open(temp.path()).unwrap();
        let supervisor = RealChildSupervisor {
            child: None,
            executable_sha256,
        };
        let mut backend = DaemonCanaryJobBackend::new(supervisor);
        let transition = launch_canary_job(&store, &job, 2, &mut backend).unwrap();
        assert!(transition.launched);
        let process = match &transition.snapshot.latest().receipt {
            crate::parallel_proof_canary_job::CanaryJobReceiptState::Running { process } => process,
            other => panic!("expected running receipt, got {other:?}"),
        };
        assert_eq!(
            process.launch_nonce_sha256,
            match &transition.snapshot.receipts[0].receipt {
                crate::parallel_proof_canary_job::CanaryJobReceiptState::Prepared {
                    launch_nonce_sha256,
                } => launch_nonce_sha256.clone(),
                _ => unreachable!(),
            }
        );
        assert_eq!(
            backend.cancel(&job, process, 1_000).unwrap(),
            CanaryCancellationObservation::Terminated
        );
    }

    #[test]
    fn capability_is_exact_and_default_off() {
        assert!(!daemon_supports_canary_jobs(&serde_json::json!({})));
        assert!(!daemon_supports_canary_jobs(&serde_json::json!({
            "capabilities": ["parallel_proof_canary_job_v2"]
        })));
        assert!(daemon_supports_canary_jobs(&serde_json::json!({
            "capabilities": [DAEMON_CANARY_JOB_CAPABILITY]
        })));
    }

    #[test]
    fn bounded_batches_rotate_fairly_across_backlog() {
        let jobs = (0..5)
            .map(|index| format!("job-{index}"))
            .collect::<Vec<_>>();
        assert_eq!(bounded_job_batch(&jobs, None, 1), vec!["job-0"]);
        assert_eq!(bounded_job_batch(&jobs, Some("job-0"), 1), vec!["job-1"]);
        assert_eq!(bounded_job_batch(&jobs, Some("job-4"), 1), vec!["job-0"]);
        assert!(bounded_job_batch(&jobs, None, 0).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_refuses_mismatched_os_start_identity_without_signalling() {
        let _guard = crate::test_support::PROCESS_TREE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().unwrap();
        let binary = PathBuf::from("/bin/sleep");
        let binary_sha256 = executable_digest(&binary).unwrap();
        let job = job(binary_sha256.clone());
        let nonce = digest("pid-reuse-negative-control");
        let mut child = Command::new(&binary).arg("30").spawn().unwrap();
        let pid = child.id();
        let process = CanaryProcessTreeIdentity {
            pid,
            tree_id: format!("pgrp-{pid}"),
            birth_token: nonce.as_str().to_owned(),
            os_start_identity_sha256: digest("different-process-start"),
            launch_nonce_sha256: nonce.clone(),
            executable_sha256: binary_sha256,
            launched_at_ms: 2,
        };
        let mut supervisor = ShipyardCanaryProcessSupervisor::new(
            binary,
            RuntimeMode::Isolated,
            temp.path().join("global"),
            temp.path().join("state"),
        )
        .unwrap();
        supervisor
            .store
            .put_receipt(&CanarySupervisorReceipt {
                job_id: job.job_id.clone(),
                generation: nonce.as_str().to_owned(),
                process: process.clone(),
            })
            .unwrap();

        assert_eq!(
            supervisor.cancel_typed_worker(&job, &process, 100).unwrap(),
            CanaryCancellationObservation::Missing
        );
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn production_supervisor_restarts_without_redispatch_and_cancels_tree() {
        let _guard = crate::test_support::PROCESS_TREE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = tempfile::tempdir().unwrap();
        let descendant_pid_path = temp.path().join("detached-descendant.pid");
        let descendant_pid_path_literal = format!("{descendant_pid_path:?}");
        let source = format!(
            r#"use std::os::unix::process::CommandExt as _;
fn main() {{
    let mut command = std::process::Command::new("/bin/sleep");
    command.arg("30").process_group(0);
    let child = command.spawn().unwrap();
    std::fs::write({descendant_pid_path_literal}, child.id().to_string()).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(30));
}}"#,
        );
        let binary = crate::test_support::compile_native_test_program(
            temp.path(),
            "typed_canary_worker_fixture",
            &source,
        );
        let binary_sha256 = executable_digest(&binary).unwrap();
        let job = job(binary_sha256);
        let nonce = digest("restart-nonce");
        let request = CanarySupervisedLaunch {
            job: job.clone(),
            job_sha256: job.digest().unwrap(),
            launch_nonce_sha256: nonce.clone(),
            claimed_at_ms: 2,
        };
        let state_dir = temp.path().join("state");
        let global_dir = temp.path().join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        let mut first = ShipyardCanaryProcessSupervisor::new(
            binary.clone(),
            RuntimeMode::Isolated,
            global_dir.clone(),
            state_dir.clone(),
        )
        .unwrap();
        let process = first.launch_typed_worker(&request).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !descendant_pid_path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let descendant_pid = std::fs::read_to_string(&descendant_pid_path)
            .unwrap()
            .parse::<u32>()
            .unwrap();
        drop(first);

        let mut restarted = ShipyardCanaryProcessSupervisor::new(
            binary,
            RuntimeMode::Isolated,
            global_dir,
            state_dir,
        )
        .unwrap();
        assert!(matches!(
            restarted.discover_typed_worker(&job, &nonce).unwrap(),
            CanaryProcessObservation::Alive(observed) if observed == process
        ));
        assert_eq!(
            restarted.cancel_typed_worker(&job, &process, 500).unwrap(),
            CanaryCancellationObservation::Terminated
        );
        let descendant_status = Command::new("/bin/ps")
            .args(["-p", &descendant_pid.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        assert!(
            !descendant_status.status.success()
                || String::from_utf8_lossy(&descendant_status.stdout)
                    .trim_start()
                    .starts_with('Z')
        );
        assert!(matches!(
            restarted.observe_typed_worker(&job, &process).unwrap(),
            CanaryProcessObservation::Missing
        ));
    }

    #[cfg(unix)]
    #[test]
    fn production_supervisor_missing_receipt_never_launches_during_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let binary = crate::test_support::compile_native_test_program(
            temp.path(),
            "typed_canary_missing_fixture",
            "fn main() {}",
        );
        let binary_sha256 = executable_digest(&binary).unwrap();
        let job = job(binary_sha256);
        let mut supervisor = ShipyardCanaryProcessSupervisor::new(
            binary,
            RuntimeMode::Isolated,
            temp.path().join("global"),
            temp.path().join("state"),
        )
        .unwrap();
        assert!(matches!(
            supervisor
                .discover_typed_worker(&job, &digest("never-launched"))
                .unwrap(),
            CanaryProcessObservation::Missing
        ));
    }
}
