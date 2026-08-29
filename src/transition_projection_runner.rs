//! Default-off external runner for the local transition projection outbox.
//!
//! This module is deliberately downstream of stewardship. Producers first
//! commit their authoritative receipt, then use [`CommittedTransitionIngress`]
//! to verify that exact receipt and enqueue a projection. The daemon only
//! drains already durable records; projection failure never changes custody,
//! queue, merge, or continuation authority.

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

const POLICY_KEY: &str = "transition_projection";
const PROTOCOL_VERSION: u32 = 1;
const MAX_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SECRET_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PROTOCOL_BYTES: u64 = 1024 * 1024;
const PROJECTION_DRAIN_INTERVAL: Duration = Duration::from_secs(5);
const PROJECTION_LEASE_SAFETY_MS: u64 = 5_000;
const PROJECTION_CALLS_PER_RECONCILE: u64 = 2;
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
    ) -> Result<EnqueueOutcome, String> {
        let Some(root) = &self.root else {
            return Ok(EnqueueOutcome::Disabled);
        };
        if !self.repositories.contains(repository) {
            return Err("transition projection repository is not authorized".to_owned());
        }
        validate_repository(repository)?;
        let observed = digest_private_receipt(source_receipt_path)?;
        if observed != draft.evidence.receipt_sha256 {
            return Err("transition projection source receipt digest mismatch".to_owned());
        }
        open_repository_outbox(root, repository)
            .map_err(|error| error.to_string())?
            .enqueue(draft)
            .map_err(|error| error.to_string())
    }
}

/// Best-effort daemon status. Errors are diagnostic only.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectionRunnerStatus {
    pub(crate) enabled: bool,
    pub(crate) last_outcome: Option<String>,
    pub(crate) last_error: Option<String>,
}

pub(crate) struct TransitionProjectionRuntime {
    state_dir: PathBuf,
    config: Option<ProjectionRunnerConfig>,
    status: ProjectionRunnerStatus,
    result_rx: Option<Receiver<Vec<Result<ReconcileOutcome, ()>>>>,
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
                Ok(results) => {
                    self.result_rx = None;
                    self.next_run_at = Instant::now() + PROJECTION_DRAIN_INTERVAL;
                    for result in results {
                        match result {
                            Ok(outcome) => {
                                self.status.last_error = None;
                                self.status.last_outcome = Some(outcome_name(&outcome).to_owned());
                            }
                            Err(()) => {
                                self.status.last_error =
                                    Some("projection-drain-refused".to_owned());
                            }
                        }
                    }
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
        let root = self.state_dir.join("transition-projection");
        let repositories = config.repositories.iter().cloned().collect::<Vec<_>>();
        let (sender, receiver) = mpsc::channel();
        self.result_rx = Some(receiver);
        self.next_run_at = Instant::now() + PROJECTION_DRAIN_INTERVAL;
        thread::spawn(move || {
            let now = unix_ms();
            let results = repositories
                .into_iter()
                .map(|repository| {
                    let outbox = open_repository_outbox(&root, &repository).map_err(|_| ())?;
                    let mut adapter = CompanionAdapter::new(config.clone());
                    outbox.reconcile_one(&mut adapter, now).map_err(|_| ())
                })
                .collect();
            let _ = sender.send(results);
        });
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
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::config::LocalOverlaySource;
    use crate::transition_projection::{ProjectionEvidence, TransitionKind};

    fn draft(receipt: &[u8], sequence: u64, kind: TransitionKind) -> TransitionDraft {
        TransitionDraft {
            workstream_id: "GEN-14".to_owned(),
            sequence,
            kind,
            evidence: ProjectionEvidence {
                source_revision: "a".repeat(64),
                exact_head: Some("b".repeat(40)),
                receipt_sha256: hex::encode(Sha256::digest(receipt)),
            },
            supersedes_transition_id: None,
            note: Some("safe".to_owned()),
        }
    }

    fn policy(executable: &Path, digest: &str, secret: &Path) -> String {
        format!(
            "[transition_projection]\nenabled = true\nexecutable_path = \"{}\"\nexecutable_sha256 = \"{digest}\"\nargv = [\"linear-v1\"]\ndeadline_seconds = 2\nmax_stdout_bytes = 4096\nmax_stderr_bytes = 4096\nrepositories = [\"owner/repo\"]\n[transition_projection.secret_files]\nLINEAR_API_KEY_FILE = \"{}\"\n",
            executable.display(),
            secret.display()
        )
    }

    #[test]
    fn disabled_and_unavailable_have_zero_stewardship_effect() {
        let temp = tempfile::tempdir().unwrap();
        let config = trusted_projection_runner_config(RuntimeMode::Shipyard, temp.path().into())
            .expect("absent policy");
        assert_eq!(config, None);
        let runtime = TransitionProjectionRuntime::for_daemon(
            RuntimeMode::Shipyard,
            temp.path().into(),
            temp.path().join("state"),
        );
        let result = runtime.ingress().enqueue_after_commit(
            "not/a/repo",
            draft(b"missing", 1, TransitionKind::Handoff),
            Path::new("/missing"),
        );
        assert_eq!(result.unwrap(), EnqueueOutcome::Disabled);
        assert!(!temp.path().join("state/transition-projection").exists());
    }

    #[test]
    fn protected_config_ignores_overlays_and_rejects_secret_argv() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        let project = temp.path().join("project");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("config.toml"),
            "[transition_projection]\nenabled=true\n",
        )
        .unwrap();
        let loaded = LoadedConfig::load(
            Some(global.clone()),
            Some(project),
            None,
            LocalOverlaySource::None,
        )
        .unwrap();
        assert!(loaded.get(POLICY_KEY).is_some());
        assert_eq!(
            trusted_projection_runner_config(RuntimeMode::Shipyard, global).unwrap(),
            None
        );

        let secret = temp.path().join("linear-key");
        fs::write(&secret, b"not-read-at-config-time").unwrap();
        fs::write(
            temp.path().join("config.toml"),
            policy(Path::new("/bin/false"), &"a".repeat(64), &secret),
        )
        .unwrap();
        assert!(
            trusted_projection_runner_config(RuntimeMode::Shipyard, temp.path().into())
                .unwrap()
                .is_some()
        );

        let over_lease = policy(Path::new("/bin/false"), &"a".repeat(64), &secret)
            .replace("deadline_seconds = 2", "deadline_seconds = 28");
        fs::write(temp.path().join("config.toml"), over_lease).unwrap();
        assert!(
            trusted_projection_runner_config(RuntimeMode::Shipyard, temp.path().into()).is_err()
        );

        let bad = "[transition_projection]\nenabled=true\nexecutable_path=\"/bin/x\"\nexecutable_sha256=\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nargv=[\"token=secret\"]\ndeadline_seconds=1\nmax_stdout_bytes=1\nmax_stderr_bytes=1\nrepositories=[\"owner/repo\"]\n";
        fs::write(temp.path().join("config.toml"), bad).unwrap();
        assert!(
            trusted_projection_runner_config(RuntimeMode::Shipyard, temp.path().into()).is_err()
        );
    }

    #[test]
    #[cfg(unix)]
    fn commit_before_enqueue_and_repository_partition_are_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let receipt_path = temp.path().join("receipt.json");
        fs::write(&receipt_path, b"committed-receipt").unwrap();
        fs::set_permissions(&receipt_path, fs::Permissions::from_mode(0o600)).unwrap();
        let config = ProjectionRunnerConfig {
            executable_path: "/bin/false".into(),
            executable_sha256: "a".repeat(64),
            argv: vec!["linear-v1".into()],
            secret_files: BTreeMap::new(),
            deadline_seconds: 1,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            repositories: BTreeSet::from(["owner/repo".to_owned()]),
        };
        let ingress = CommittedTransitionIngress::enabled(temp.path(), &config);
        assert!(
            ingress
                .enqueue_after_commit(
                    "other/repo",
                    draft(b"committed-receipt", 1, TransitionKind::Waiting),
                    &receipt_path,
                )
                .is_err()
        );
        assert!(
            ingress
                .enqueue_after_commit(
                    "owner/repo",
                    draft(b"wrong", 1, TransitionKind::Waiting),
                    &receipt_path,
                )
                .is_err()
        );
        assert_eq!(
            ingress
                .enqueue_after_commit(
                    "owner/repo",
                    draft(b"committed-receipt", 1, TransitionKind::Waiting),
                    &receipt_path,
                )
                .unwrap(),
            EnqueueOutcome::Queued
        );
        let expected = repository_outbox(&temp.path().join("transition-projection"), "owner/repo");
        assert!(expected.join("transitions.ndjson").is_file());
        assert!(
            !temp
                .path()
                .join("transition-projection/repositories/owner/repo")
                .exists()
        );
    }

    #[test]
    #[cfg(unix)]
    fn every_allowed_transition_kind_uses_the_same_committed_ingress() {
        let temp = tempfile::tempdir().unwrap();
        let config = ProjectionRunnerConfig {
            executable_path: "/bin/false".into(),
            executable_sha256: "a".repeat(64),
            argv: vec!["linear-v1".into()],
            secret_files: BTreeMap::new(),
            deadline_seconds: 1,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            repositories: BTreeSet::from(["owner/repo".to_owned()]),
        };
        let ingress = CommittedTransitionIngress::enabled(temp.path(), &config);
        for (index, kind) in [
            TransitionKind::Handoff,
            TransitionKind::Waiting,
            TransitionKind::Actionable,
            TransitionKind::NewHead,
            TransitionKind::Merge,
            TransitionKind::ConfiguredClosure,
        ]
        .into_iter()
        .enumerate()
        {
            let bytes = format!("receipt-{index}");
            let path = temp.path().join(format!("receipt-{index}.json"));
            fs::write(&path, bytes.as_bytes()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                ingress
                    .enqueue_after_commit(
                        "owner/repo",
                        draft(bytes.as_bytes(), index as u64 + 1, kind),
                        &path
                    )
                    .unwrap(),
                EnqueueOutcome::Queued
            );
        }
    }

    #[test]
    fn malformed_protocol_response_is_retryable_and_secret_free() {
        let failure = serde_json::from_slice::<ProtocolResponse>(b"{not-json")
            .map_err(|_| adapter_failure("malformed-response token=secret", true))
            .unwrap_err();
        assert!(failure.retryable);
        assert!(!failure.reason.contains("secret"));
        assert!(!failure.reason.contains("token"));
    }

    #[test]
    #[cfg(unix)]
    fn daemon_restart_reopens_same_repository_outbox() {
        let temp = tempfile::tempdir().unwrap();
        let receipt_path = temp.path().join("receipt");
        fs::write(&receipt_path, b"receipt").unwrap();
        fs::set_permissions(&receipt_path, fs::Permissions::from_mode(0o600)).unwrap();
        let config = ProjectionRunnerConfig {
            executable_path: "/bin/false".into(),
            executable_sha256: "a".repeat(64),
            argv: vec!["linear-v1".into()],
            secret_files: BTreeMap::new(),
            deadline_seconds: 1,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            repositories: BTreeSet::from(["owner/repo".to_owned()]),
        };
        CommittedTransitionIngress::enabled(temp.path(), &config)
            .enqueue_after_commit(
                "owner/repo",
                draft(b"receipt", 1, TransitionKind::Actionable),
                &receipt_path,
            )
            .unwrap();
        let root = temp.path().join("transition-projection");
        let first = TransitionOutbox::open(repository_outbox(&root, "owner/repo")).unwrap();
        let second = TransitionOutbox::open(repository_outbox(&root, "owner/repo")).unwrap();
        assert_eq!(first.snapshot().unwrap(), second.snapshot().unwrap());
    }

    #[test]
    fn crash_after_external_acceptance_replays_same_key_then_acks() {
        struct AcceptedThenReadable {
            accepted: Arc<Mutex<BTreeMap<String, String>>>,
            fail_readback_once: bool,
        }

        impl TransitionProjectionAdapter for AcceptedThenReadable {
            fn submit(
                &mut self,
                transition: &ProjectedTransition,
            ) -> Result<SubmitReceipt, AdapterFailure> {
                self.accepted
                    .lock()
                    .unwrap()
                    .entry(transition.transition_id.clone())
                    .or_insert_with(|| transition.evidence_identity.clone());
                Ok(SubmitReceipt {
                    external_id: "external-1".into(),
                    idempotency_key: transition.transition_id.clone(),
                })
            }

            fn readback(
                &mut self,
                receipt: &SubmitReceipt,
            ) -> Result<ProjectionReadback, AdapterFailure> {
                if self.fail_readback_once {
                    self.fail_readback_once = false;
                    return Err(adapter_failure("accepted-before-crash", true));
                }
                Ok(ProjectionReadback {
                    transition_id: receipt.idempotency_key.clone(),
                    evidence_identity: self.accepted.lock().unwrap()[&receipt.idempotency_key]
                        .clone(),
                })
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let outbox = TransitionOutbox::open(&root).unwrap();
        outbox
            .enqueue(draft(b"receipt", 1, TransitionKind::Merge))
            .unwrap();
        let accepted = Arc::new(Mutex::new(BTreeMap::new()));
        let mut first = AcceptedThenReadable {
            accepted: Arc::clone(&accepted),
            fail_readback_once: true,
        };
        assert!(matches!(
            outbox.reconcile_one(&mut first, 1).unwrap(),
            ReconcileOutcome::RetryQueued { .. }
        ));
        drop(outbox);
        let reopened = TransitionOutbox::open(root).unwrap();
        let mut recovered = AcceptedThenReadable {
            accepted: Arc::clone(&accepted),
            fail_readback_once: false,
        };
        assert!(matches!(
            reopened.reconcile_one(&mut recovered, 1_001).unwrap(),
            ReconcileOutcome::Acknowledged { .. }
        ));
        assert_eq!(accepted.lock().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn companion_receives_descriptor_bound_secret_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let secret = temp.path().join("linear-key");
        fs::write(&secret, b"original-secret").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let snapshot_dir = temp.path().join("snapshot");
        fs::create_dir(&snapshot_dir).unwrap();
        fs::set_permissions(&snapshot_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let config = ProjectionRunnerConfig {
            executable_path: "/bin/false".into(),
            executable_sha256: "a".repeat(64),
            argv: Vec::new(),
            secret_files: BTreeMap::from([("LINEAR_API_KEY_FILE".to_owned(), secret.clone())]),
            deadline_seconds: 1,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            repositories: BTreeSet::from(["owner/repo".to_owned()]),
        };
        let environment = snapshot_secret_files(
            &config,
            &snapshot_dir,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        fs::write(secret, b"replacement-secret").unwrap();
        let snapshot = PathBuf::from(&environment["LINEAR_API_KEY_FILE"]);
        assert_eq!(fs::read(snapshot).unwrap(), b"original-secret");
    }

    #[cfg(unix)]
    #[test]
    fn companion_timeout_is_bounded_and_descendant_safe() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("projection-adapter");
        let source = temp.path().join("projection-adapter.c");
        fs::write(
            &source,
            "#include <unistd.h>\nint main(void) { sleep(5); return 0; }\n",
        )
        .unwrap();
        assert!(
            Command::new("/usr/bin/cc")
                .args(["-o"])
                .arg(&executable)
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o500)).unwrap();
        let digest = hex::encode(Sha256::digest(fs::read(&executable).unwrap()));
        let config = ProjectionRunnerConfig {
            executable_path: executable,
            executable_sha256: digest,
            argv: vec!["5".into()],
            secret_files: BTreeMap::new(),
            deadline_seconds: 1,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            repositories: BTreeSet::from(["owner/repo".to_owned()]),
        };
        let started = Instant::now();
        assert_eq!(
            run_companion(&config, b"{}"),
            Err("companion-timeout-or-output-limit")
        );
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
