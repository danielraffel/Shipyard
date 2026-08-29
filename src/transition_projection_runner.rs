//! Default-off external runner for the local transition projection outbox.
//!
//! This module is deliberately downstream of stewardship. Producers first
//! commit their authoritative receipt, then use [`CommittedTransitionIngress`]
//! to verify that exact receipt and enqueue a projection. The daemon only
//! drains already durable records; projection failure never changes custody,
//! queue, merge, or continuation authority.

mod drain;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::transition_projection::{
    AdapterFailure, EnqueueOutcome, PROJECTION_CLAIM_LEASE_MS, ProjectedTransition,
    ProjectionReadback, ReconcileOutcome, SubmitReceipt, TransitionDraft, TransitionOutbox,
    TransitionProjectionAdapter,
};
#[cfg(test)]
use crate::work_ledger::WorkLedger;

#[cfg(test)]
use self::drain::ProjectionDrainFailureKind;
use self::drain::{
    CommittedEnqueueError, ProjectionIntentDrainReport, drain_committed_projection_intents,
};

const POLICY_KEY: &str = "transition_projection";
const PROTOCOL_VERSION: u32 = 1;
const MAX_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SECRET_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PROTOCOL_BYTES: u64 = 1024 * 1024;
const PROJECTION_DRAIN_INTERVAL: Duration = Duration::from_secs(5);
const PROJECTION_LEASE_SAFETY_MS: u64 = 5_000;
const PROJECTION_CALLS_PER_RECONCILE: u64 = 2;
const MAX_COMMITTED_INTENTS_PER_DRAIN: u64 = 32;
#[allow(dead_code)] // Activated by the authoritative producer follow-up.
const MAX_RECEIPT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ARGS: usize = 16;
const MAX_SECRET_FILES: usize = 16;

/// Protected machine-global policy for one external projection companion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectionRunnerConfig {
    executable_path: PathBuf,
    executable_sha256: String,
    argv: Vec<String>,
    secret_files: BTreeMap<String, PathBuf>,
    deadline_seconds: u64,
    max_stdout_bytes: u64,
    max_stderr_bytes: u64,
    repositories: BTreeSet<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    enabled: bool,
    executable_path: Option<String>,
    executable_sha256: Option<String>,
    argv: Option<Vec<String>>,
    secret_files: Option<BTreeMap<String, String>>,
    deadline_seconds: Option<u64>,
    max_stdout_bytes: Option<u64>,
    max_stderr_bytes: Option<u64>,
    repositories: Option<Vec<String>>,
}

pub(crate) fn trusted_projection_runner_config(
    mode: RuntimeMode,
    global_dir: PathBuf,
) -> Result<Option<ProjectionRunnerConfig>, String> {
    if mode != RuntimeMode::Shipyard {
        return Ok(None);
    }
    let trusted = LoadedConfig::load_machine_global_from_dir(global_dir)
        .map_err(|_| "load trusted transition projection policy".to_owned())?;
    let Some(value) = trusted.get(POLICY_KEY) else {
        return Ok(None);
    };
    let raw: RawPolicy = value
        .clone()
        .try_into()
        .map_err(|_| "decode trusted transition projection policy".to_owned())?;
    if !raw.enabled {
        if raw.executable_path.is_some()
            || raw.executable_sha256.is_some()
            || raw.argv.is_some()
            || raw.secret_files.is_some()
            || raw.deadline_seconds.is_some()
            || raw.max_stdout_bytes.is_some()
            || raw.max_stderr_bytes.is_some()
            || raw.repositories.is_some()
        {
            return Err("disabled transition projection contains activation fields".to_owned());
        }
        return Ok(None);
    }
    validate_enabled(raw).map(Some)
}

fn validate_enabled(raw: RawPolicy) -> Result<ProjectionRunnerConfig, String> {
    let executable_path = normalized_absolute(required(raw.executable_path, "executable_path")?)?;
    let executable_sha256 = required(raw.executable_sha256, "executable_sha256")?;
    validate_digest(&executable_sha256)?;
    let argv = required(raw.argv, "argv")?;
    if argv.is_empty()
        || argv.len() > MAX_ARGS
        || argv.iter().any(|arg| {
            arg.is_empty()
                || arg.len() > 128
                || !arg.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
                })
        })
    {
        return Err("transition projection argv is not a fixed bounded token list".to_owned());
    }
    let mut secret_files = BTreeMap::new();
    for (name, path) in raw.secret_files.unwrap_or_default() {
        if secret_files.len() >= MAX_SECRET_FILES
            || !name.ends_with("_FILE")
            || name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err("transition projection secret-file environment is invalid".to_owned());
        }
        secret_files.insert(name, normalized_absolute(path)?);
    }
    let deadline_seconds = required(raw.deadline_seconds, "deadline_seconds")?;
    let max_stdout_bytes = required(raw.max_stdout_bytes, "max_stdout_bytes")?;
    let max_stderr_bytes = required(raw.max_stderr_bytes, "max_stderr_bytes")?;
    let reconcile_budget_ms = deadline_seconds
        .saturating_mul(1_000)
        .saturating_mul(PROJECTION_CALLS_PER_RECONCILE)
        .saturating_add(PROJECTION_LEASE_SAFETY_MS);
    if deadline_seconds == 0
        || reconcile_budget_ms >= PROJECTION_CLAIM_LEASE_MS
        || !(1..=MAX_PROTOCOL_BYTES).contains(&max_stdout_bytes)
        || !(1..=MAX_PROTOCOL_BYTES).contains(&max_stderr_bytes)
    {
        return Err("transition projection execution bounds are invalid".to_owned());
    }
    let repositories = required(raw.repositories, "repositories")?;
    if repositories.is_empty() {
        return Err("transition projection repository allowlist is empty".to_owned());
    }
    let mut canonical = BTreeSet::new();
    for repository in repositories {
        validate_repository(&repository)?;
        if !canonical.insert(repository) {
            return Err("transition projection repository allowlist has duplicates".to_owned());
        }
    }
    Ok(ProjectionRunnerConfig {
        executable_path,
        executable_sha256,
        argv,
        secret_files,
        deadline_seconds,
        max_stdout_bytes,
        max_stderr_bytes,
        repositories: canonical,
    })
}

/// Commit-before-enqueue boundary used by authoritative producers.
#[allow(dead_code)] // The next integration pass wires authoritative producers.
pub(crate) struct CommittedTransitionIngress {
    root: Option<PathBuf>,
    repositories: BTreeSet<String>,
}

#[allow(dead_code)]
impl CommittedTransitionIngress {
    pub(crate) fn disabled() -> Self {
        Self {
            root: None,
            repositories: BTreeSet::new(),
        }
    }

    pub(crate) fn enabled(state_dir: &Path, config: &ProjectionRunnerConfig) -> Self {
        Self {
            root: Some(state_dir.join("transition-projection")),
            repositories: config.repositories.clone(),
        }
    }

    /// Verify the exact already-durable source receipt before appending to the
    /// repository-partitioned outbox. Disabled mode has zero validation or I/O.
    pub(crate) fn enqueue_after_commit(
        &self,
        repository: &str,
        draft: TransitionDraft,
        source_receipt_path: &Path,
    ) -> Result<EnqueueOutcome, CommittedEnqueueError> {
        let Some(root) = &self.root else {
            return Ok(EnqueueOutcome::Disabled);
        };
        if !self.repositories.contains(repository) {
            return Err(CommittedEnqueueError::contradiction(
                "transition projection repository is not authorized",
            ));
        }
        validate_repository(repository).map_err(CommittedEnqueueError::contradiction)?;
        let observed = digest_private_receipt(source_receipt_path)
            .map_err(CommittedEnqueueError::transient)?;
        if observed != draft.evidence.receipt_sha256 {
            return Err(CommittedEnqueueError::contradiction(
                "transition projection source receipt digest mismatch",
            ));
        }
        open_repository_outbox(root, repository)
            .map_err(CommittedEnqueueError::from)?
            .enqueue(draft)
            .map_err(CommittedEnqueueError::from)
    }

    /// Append a draft reconstructed from the immutable `SQLite` receipt snapshot.
    pub(crate) fn enqueue_committed_snapshot(
        &self,
        repository: &str,
        draft: TransitionDraft,
        receipt_snapshot: &[u8],
    ) -> Result<EnqueueOutcome, CommittedEnqueueError> {
        let Some(root) = &self.root else {
            return Ok(EnqueueOutcome::Disabled);
        };
        if !self.repositories.contains(repository) {
            return Err(CommittedEnqueueError::contradiction(
                "transition projection repository is not authorized",
            ));
        }
        validate_repository(repository).map_err(CommittedEnqueueError::contradiction)?;
        if receipt_snapshot.len() as u64 > MAX_RECEIPT_BYTES {
            return Err(CommittedEnqueueError::contradiction(
                "transition projection receipt snapshot exceeds its bound",
            ));
        }
        let observed = hex::encode(Sha256::digest(receipt_snapshot));
        if observed != draft.evidence.receipt_sha256 {
            return Err(CommittedEnqueueError::contradiction(
                "transition projection source receipt digest mismatch",
            ));
        }
        open_repository_outbox(root, repository)
            .map_err(CommittedEnqueueError::from)?
            .enqueue(draft)
            .map_err(CommittedEnqueueError::from)
    }
}

/// Best-effort daemon status. Errors are diagnostic only.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectionRunnerStatus {
    pub(crate) enabled: bool,
    pub(crate) last_outcome: Option<String>,
    pub(crate) last_error: Option<String>,
}

struct ProjectionWorkerReport {
    intent_drain: ProjectionIntentDrainReport,
    reconciliations: Vec<Result<ReconcileOutcome, ()>>,
}

pub(crate) struct TransitionProjectionRuntime {
    state_dir: PathBuf,
    config: Option<ProjectionRunnerConfig>,
    status: ProjectionRunnerStatus,
    result_rx: Option<Receiver<ProjectionWorkerReport>>,
    next_run_at: Instant,
}

impl TransitionProjectionRuntime {
    pub(crate) fn for_daemon(mode: RuntimeMode, global_dir: PathBuf, state_dir: PathBuf) -> Self {
        match trusted_projection_runner_config(mode, global_dir) {
            Ok(config) => Self {
                state_dir,
                status: ProjectionRunnerStatus {
                    enabled: config.is_some(),
                    ..ProjectionRunnerStatus::default()
                },
                config,
                result_rx: None,
                next_run_at: Instant::now(),
            },
            Err(error) => Self {
                state_dir,
                config: None,
                status: ProjectionRunnerStatus {
                    enabled: false,
                    last_outcome: None,
                    last_error: Some(error),
                },
                result_rx: None,
                next_run_at: Instant::now(),
            },
        }
    }

    #[allow(dead_code)] // Returned to authoritative producers in the next pass.
    pub(crate) fn ingress(&self) -> CommittedTransitionIngress {
        self.config
            .as_ref()
            .map_or_else(CommittedTransitionIngress::disabled, |config| {
                CommittedTransitionIngress::enabled(&self.state_dir, config)
            })
    }

    pub(crate) fn diagnostic_error(&self) -> Option<String> {
        self.status.last_error.clone()
    }

    /// Drain at most one transition per repository. Never returns an error to
    /// the daemon or its stewardship lanes.
    pub(crate) fn tick(&mut self) {
        if let Some(receiver) = &self.result_rx {
            match receiver.try_recv() {
                Ok(report) => {
                    self.result_rx = None;
                    self.next_run_at = Instant::now() + PROJECTION_DRAIN_INTERVAL;
                    apply_worker_report(&mut self.status, report);
                }
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result_rx = None;
                    self.next_run_at = Instant::now() + PROJECTION_DRAIN_INTERVAL;
                    self.status.last_error = Some("projection-worker-lost".to_owned());
                }
            }
        }
        let Some(config) = &self.config else {
            return;
        };
        if self.result_rx.is_some() {
            return;
        }
        if Instant::now() < self.next_run_at {
            return;
        }
        let config = config.clone();
        let state_dir = self.state_dir.clone();
        let root = self.state_dir.join("transition-projection");
        let repositories = config.repositories.iter().cloned().collect::<Vec<_>>();
        let (sender, receiver) = mpsc::channel();
        self.result_rx = Some(receiver);
        self.next_run_at = Instant::now() + PROJECTION_DRAIN_INTERVAL;
        thread::spawn(move || {
            let now = unix_ms();
            let ingress = CommittedTransitionIngress::enabled(&state_dir, &config);
            let intent_drain = drain_committed_projection_intents(&state_dir, &ingress, now);
            let reconciliations = repositories
                .into_iter()
                .map(|repository| {
                    let outbox = open_repository_outbox(&root, &repository).map_err(|_| ())?;
                    let mut adapter = CompanionAdapter::new(config.clone());
                    outbox.reconcile_one(&mut adapter, now).map_err(|_| ())
                })
                .collect();
            let _ = sender.send(ProjectionWorkerReport {
                intent_drain,
                reconciliations,
            });
        });
    }
}

fn apply_worker_report(status: &mut ProjectionRunnerStatus, report: ProjectionWorkerReport) {
    status.last_error = report.intent_drain.diagnostic_error();
    for result in report.reconciliations {
        match result {
            Ok(outcome) => status.last_outcome = Some(outcome_name(&outcome).to_owned()),
            Err(()) => {
                status
                    .last_error
                    .get_or_insert_with(|| "projection-drain-refused".to_owned());
            }
        }
    }
}

fn outcome_name(outcome: &ReconcileOutcome) -> &'static str {
    match outcome {
        ReconcileOutcome::Disabled => "disabled",
        ReconcileOutcome::Idle => "idle",
        ReconcileOutcome::Acknowledged { .. } => "acknowledged",
        ReconcileOutcome::RetryQueued { .. } => "retry_queued",
        ReconcileOutcome::Refused { .. } => "refused",
    }
}

#[derive(Clone)]
struct CompanionAdapter {
    config: ProjectionRunnerConfig,
}

impl CompanionAdapter {
    fn new(config: ProjectionRunnerConfig) -> Self {
        Self { config }
    }

    fn invoke(&self, request: &ProtocolRequest<'_>) -> Result<ProtocolResponse, AdapterFailure> {
        let bytes = serde_json::to_vec(request).map_err(|_| adapter_failure("encode", false))?;
        if bytes.len() as u64 > MAX_PROTOCOL_BYTES {
            return Err(adapter_failure("request-limit", false));
        }
        let output =
            run_companion(&self.config, &bytes).map_err(|reason| adapter_failure(reason, true))?;
        serde_json::from_slice(&output).map_err(|_| adapter_failure("malformed-response", true))
    }
}

impl TransitionProjectionAdapter for CompanionAdapter {
    fn submit(
        &mut self,
        transition: &ProjectedTransition,
    ) -> Result<SubmitReceipt, AdapterFailure> {
        match self.invoke(&ProtocolRequest {
            schema_version: PROTOCOL_VERSION,
            operation: ProtocolOperation::Submit,
            transition: Some(transition),
            receipt: None,
        })? {
            ProtocolResponse::Accepted {
                schema_version: PROTOCOL_VERSION,
                operation: ProtocolOperation::Submit,
                external_id,
                idempotency_key,
                ..
            } if !external_id.is_empty() && external_id.len() <= 256 => Ok(SubmitReceipt {
                external_id,
                idempotency_key,
            }),
            ProtocolResponse::Retryable {
                schema_version: PROTOCOL_VERSION,
                operation: ProtocolOperation::Submit,
                reason_code,
            } => Err(adapter_failure(&reason_code, true)),
            ProtocolResponse::Refused {
                schema_version: PROTOCOL_VERSION,
                operation: ProtocolOperation::Submit,
                reason_code,
            } => Err(adapter_failure(&reason_code, false)),
            _ => Err(adapter_failure("response-fence-mismatch", true)),
        }
    }

    fn readback(&mut self, receipt: &SubmitReceipt) -> Result<ProjectionReadback, AdapterFailure> {
        match self.invoke(&ProtocolRequest {
            schema_version: PROTOCOL_VERSION,
            operation: ProtocolOperation::Readback,
            transition: None,
            receipt: Some(receipt),
        })? {
            ProtocolResponse::Readback {
                schema_version: PROTOCOL_VERSION,
                operation: ProtocolOperation::Readback,
                transition_id,
                evidence_identity,
            } => Ok(ProjectionReadback {
                transition_id,
                evidence_identity,
            }),
            ProtocolResponse::Retryable {
                schema_version: PROTOCOL_VERSION,
                operation: ProtocolOperation::Readback,
                reason_code,
            } => Err(adapter_failure(&reason_code, true)),
            ProtocolResponse::Refused {
                schema_version: PROTOCOL_VERSION,
                operation: ProtocolOperation::Readback,
                reason_code,
            } => Err(adapter_failure(&reason_code, false)),
            _ => Err(adapter_failure("response-fence-mismatch", true)),
        }
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ProtocolRequest<'a> {
    schema_version: u32,
    operation: ProtocolOperation,
    #[serde(skip_serializing_if = "Option::is_none")]
    transition: Option<&'a ProjectedTransition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<&'a SubmitReceipt>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProtocolOperation {
    Submit,
    Readback,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum ProtocolResponse {
    Accepted {
        schema_version: u32,
        operation: ProtocolOperation,
        external_id: String,
        idempotency_key: String,
    },
    Readback {
        schema_version: u32,
        operation: ProtocolOperation,
        transition_id: String,
        evidence_identity: String,
    },
    Retryable {
        schema_version: u32,
        operation: ProtocolOperation,
        reason_code: String,
    },
    Refused {
        schema_version: u32,
        operation: ProtocolOperation,
        reason_code: String,
    },
}

fn adapter_failure(reason: &str, retryable: bool) -> AdapterFailure {
    let canonical = !reason.is_empty()
        && reason.len() <= 128
        && reason.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    AdapterFailure {
        reason: if canonical { reason } else { "adapter-refused" }.to_owned(),
        retryable,
    }
}

#[cfg(unix)]
fn run_companion(config: &ProjectionRunnerConfig, request: &[u8]) -> Result<Vec<u8>, &'static str> {
    let deadline = Instant::now() + Duration::from_secs(config.deadline_seconds);
    let (directory, snapshot_path) = snapshot_companion(config, deadline)?;
    let environment = snapshot_secret_files(config, directory.path(), deadline)?;
    if Instant::now() >= deadline {
        return Err("companion-timeout-or-output-limit");
    }
    let mut command = Command::new(&snapshot_path);
    command
        .args(&config.argv)
        .env_clear()
        .envs(environment)
        .current_dir("/");
    let output = crate::process::run_output_with_input_until(
        &mut command,
        request,
        deadline,
        "transition projection companion",
    )
    .map_err(|_| "companion-timeout-or-output-limit")?;
    if output.stdout.len() as u64 > config.max_stdout_bytes
        || output.stderr.len() as u64 > config.max_stderr_bytes
    {
        return Err("companion-output-limit");
    }
    if !output.status.success() {
        return Err("companion-refused");
    }
    Ok(output.stdout)
}

#[cfg(unix)]
fn snapshot_secret_files(
    config: &ProjectionRunnerConfig,
    directory: &Path,
    deadline: Instant,
) -> Result<BTreeMap<String, OsString>, &'static str> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let mut environment = BTreeMap::new();
    for (index, (name, path)) in config.secret_files.iter().enumerate() {
        if Instant::now() >= deadline {
            return Err("companion-timeout-or-output-limit");
        }
        let source = OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(path)
            .map_err(|_| "secret-file-unavailable")?;
        let metadata = source.metadata().map_err(|_| "secret-file-untrusted")?;
        if !metadata.is_file()
            || metadata.uid() != nix::unistd::Uid::effective().as_raw()
            || metadata.nlink() != 1
            || metadata.len() > MAX_SECRET_FILE_BYTES
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err("secret-file-untrusted");
        }
        let snapshot_path = directory.join(format!("secret-{index}"));
        let mut snapshot = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o400)
            .open(&snapshot_path)
            .map_err(|_| "secret-snapshot-unavailable")?;
        let copied = std::io::copy(&mut source.take(MAX_SECRET_FILE_BYTES + 1), &mut snapshot)
            .map_err(|_| "secret-file-unreadable")?;
        if copied > MAX_SECRET_FILE_BYTES {
            return Err("secret-file-untrusted");
        }
        snapshot
            .sync_all()
            .map_err(|_| "secret-snapshot-unavailable")?;
        if Instant::now() >= deadline {
            return Err("companion-timeout-or-output-limit");
        }
        environment.insert(name.clone(), snapshot_path.into_os_string());
    }
    Ok(environment)
}

#[cfg(unix)]
fn snapshot_companion(
    config: &ProjectionRunnerConfig,
    deadline: Instant,
) -> Result<(tempfile::TempDir, PathBuf), &'static str> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(&config.executable_path)
        .map_err(|_| "companion-unavailable")?;
    let metadata = source.metadata().map_err(|_| "companion-untrusted")?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.nlink() != 1
        || metadata.len() == 0
        || metadata.len() > MAX_EXECUTABLE_BYTES
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err("companion-untrusted");
    }
    let directory = tempfile::tempdir().map_err(|_| "snapshot-unavailable")?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .map_err(|_| "snapshot-unavailable")?;
    let snapshot_path = directory.path().join("transition-projection-adapter");
    let mut snapshot = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o500)
        .open(&snapshot_path)
        .map_err(|_| "snapshot-unavailable")?;
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; 32 * 1024].into_boxed_slice();
    loop {
        if Instant::now() >= deadline {
            return Err("companion-timeout-or-output-limit");
        }
        let count = source
            .read(&mut buffer)
            .map_err(|_| "companion-unreadable")?;
        if count == 0 {
            break;
        }
        copied = copied.saturating_add(count as u64);
        if copied > MAX_EXECUTABLE_BYTES {
            return Err("companion-untrusted");
        }
        hasher.update(&buffer[..count]);
        snapshot
            .write_all(&buffer[..count])
            .map_err(|_| "snapshot-unavailable")?;
    }
    snapshot.sync_all().map_err(|_| "snapshot-unavailable")?;
    if Instant::now() >= deadline {
        return Err("companion-timeout-or-output-limit");
    }
    if copied != metadata.len() || hex::encode(hasher.finalize()) != config.executable_sha256 {
        return Err("companion-digest-mismatch");
    }
    drop(snapshot);
    let mut verified = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(&snapshot_path)
        .map_err(|_| "snapshot-unavailable")?;
    let mut magic = [0_u8; 4];
    verified
        .read_exact(&mut magic)
        .map_err(|_| "companion-not-native")?;
    if magic != [0x7f, b'E', b'L', b'F']
        && !matches!(
            magic,
            [0xcf, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xca, 0xfe, 0xba, 0xbe | 0xbf]
                | [0xbe | 0xbf, 0xba, 0xfe, 0xca]
        )
    {
        return Err("companion-not-native");
    }
    if Instant::now() >= deadline {
        return Err("companion-timeout-or-output-limit");
    }
    Ok((directory, snapshot_path))
}

#[cfg(not(unix))]
fn run_companion(
    _config: &ProjectionRunnerConfig,
    _request: &[u8],
) -> Result<Vec<u8>, &'static str> {
    Err("companion-platform-unavailable")
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[allow(dead_code)] // Activated by the authoritative producer follow-up.
fn digest_private_receipt(path: &Path) -> Result<String, String> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    if !path.is_absolute() || path.components().any(unsafe_component) {
        return Err("source receipt path is not normalized and absolute".to_owned());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
    let mut file = options
        .open(path)
        .map_err(|_| "source receipt is not durably readable".to_owned())?;
    let metadata = file
        .metadata()
        .map_err(|_| "source receipt metadata is unavailable".to_owned())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_RECEIPT_BYTES {
        return Err("source receipt is not a bounded regular file".to_owned());
    }
    #[cfg(unix)]
    if metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("source receipt permissions are unsafe".to_owned());
    }
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut HashWriter(&mut hasher))
        .map_err(|_| "source receipt is unreadable".to_owned())?;
    Ok(hex::encode(hasher.finalize()))
}

fn repository_outbox(root: &Path, repository: &str) -> PathBuf {
    root.join("repositories").join(hex::encode(Sha256::digest(
        format!("shipyard-transition-projection-repository-v1\0{repository}").as_bytes(),
    )))
}

fn open_repository_outbox(
    root: &Path,
    repository: &str,
) -> Result<TransitionOutbox, crate::transition_projection::ProjectionError> {
    crate::writer_domain_lease::ensure_protected_dir_all(&root.join("repositories"))?;
    TransitionOutbox::open(repository_outbox(root, repository))
}

fn validate_repository(repository: &str) -> Result<(), String> {
    let mut parts = repository.split('/');
    let valid = |value: &str| {
        !value.is_empty()
            && value.len() <= 100
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
    };
    if !valid(parts.next().unwrap_or_default())
        || !valid(parts.next().unwrap_or_default())
        || parts.next().is_some()
    {
        return Err("transition projection repository is not canonical".to_owned());
    }
    Ok(())
}

fn normalized_absolute(value: impl AsRef<str>) -> Result<PathBuf, String> {
    let value = value.as_ref();
    let path = PathBuf::from(value);
    if value.is_empty()
        || value.len() > 4096
        || !path.is_absolute()
        || path.components().any(unsafe_component)
        || path.components().collect::<PathBuf>() != path
    {
        return Err("transition projection path is not normalized and absolute".to_owned());
    }
    Ok(path)
}

fn unsafe_component(component: Component<'_>) -> bool {
    matches!(component, Component::CurDir | Component::ParentDir)
}

fn validate_digest(value: &str) -> Result<(), String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("transition projection digest is invalid".to_owned());
    }
    Ok(())
}

fn required<T>(value: Option<T>, field: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("transition projection {field} is required"))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "transition_projection_runner/tests.rs"]
mod tests;
