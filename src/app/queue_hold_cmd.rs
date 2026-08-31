//! Live local queue-admission hold for the narrow `TartCI` pool-off transition.
//!
//! The ledger is a revocation and identity fence, never admission authority.
//! Authority exists only while the exact inherited `queue.lock` file
//! description remains open in the bound child process.
//! The child transition is trusted to apply its declared aggregate scope; this
//! hold proves custody and scope identity, not individual `launchctl` arguments.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
#[cfg(target_os = "macos")]
use std::io::Read as _;
#[cfg(target_os = "macos")]
use std::io::Seek as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::process::ExitCode;
#[cfg(target_os = "macos")]
use std::thread;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

use chrono::Utc;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::parallel_proof::Sha256Digest;
#[cfg(any(target_os = "macos", all(test, unix)))]
use crate::queue::Queue;
use crate::worker_process_custody::{ProcessLiveness, process_id_liveness};

use super::CliFailure;
use super::cli::QueueHoldCommand;

const SCHEMA: u32 = 1;
const KIND: &str = "shipyard.queue-admission-hold";
const PURPOSE: &str = "tartci-pool-off";
#[cfg(target_os = "macos")]
const RETRY: Duration = Duration::from_millis(25);
const REFUSED_EXIT: u8 = 3;
#[cfg(target_os = "macos")]
const TIMEOUT_EXIT: u8 = 124;
const SETUP_EXIT: u8 = 125;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HoldStatus {
    Held,
    Revoked,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HoldRecord {
    schema: u32,
    kind: String,
    hold_id: String,
    generation: u64,
    purpose: String,
    host_id: String,
    services: Vec<String>,
    repos: Vec<String>,
    runners: Vec<String>,
    scope_digest: String,
    state_dir: PathBuf,
    lock_file: PathBuf,
    owner_pid: u32,
    owner_process_start: String,
    acquired_at: String,
    status: HoldStatus,
    revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct VerifyResponse {
    schema: u32,
    command: &'static str,
    status: &'static str,
    reason: Option<&'static str>,
    hold_id: String,
    generation: u64,
    revision: u64,
    scope_digest: String,
}

#[derive(Debug, Serialize)]
struct RevokeResponse {
    schema: u32,
    command: &'static str,
    status: &'static str,
    reason: Option<&'static str>,
    hold_id: String,
    generation: u64,
    revision: u64,
}

#[derive(Debug)]
#[cfg_attr(not(target_os = "macos"), expect(dead_code))]
struct Scope {
    purpose: String,
    host_id: String,
    services: Vec<String>,
    repos: Vec<String>,
    runners: Vec<String>,
    digest: String,
}

struct LedgerLock {
    state_dir: PathBuf,
    file: File,
}

pub(super) fn queue_hold_command<W: Write>(
    command: QueueHoldCommand,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        QueueHoldCommand::Exec {
            purpose,
            host_id,
            services,
            repos,
            runners,
            timeout_seconds,
            command,
        } => {
            if json {
                return Err(setup_failure(
                    "queue-hold exec cannot combine child stdout with --json",
                ));
            }
            exec_hold(
                state_dir,
                &Scope::new(purpose, host_id, services, repos, runners)?,
                Duration::from_secs(timeout_seconds),
                &command,
            )
        }
        QueueHoldCommand::Verify {
            hold_id,
            generation,
            host_id,
            scope_digest,
            owner_pid,
            owner_process_start,
            fd,
        } => {
            let response = verify_hold(
                state_dir,
                &hold_id,
                generation,
                &host_id,
                &scope_digest,
                owner_pid,
                &owner_process_start,
                fd,
            )?;
            render_verify(stdout, json, &response)?;
            Ok(if response.status == "held" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(REFUSED_EXIT)
            })
        }
        QueueHoldCommand::Revoke {
            hold_id,
            generation,
            reason,
        } => {
            validate_value("hold id", &hold_id)?;
            if generation == 0 {
                return Err(setup_failure("queue-hold generation must be positive"));
            }
            validate_value("revocation reason", &reason)?;
            let response = revoke_hold(state_dir, &hold_id, generation, reason.trim())?;
            render_revoke(stdout, json, &response)?;
            Ok(if response.status == "revoked" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(REFUSED_EXIT)
            })
        }
    }
}

#[cfg(target_os = "macos")]
fn exec_hold(
    state_dir: &Path,
    scope: &Scope,
    timeout: Duration,
    command: &[OsString],
) -> Result<ExitCode, CliFailure> {
    use std::os::unix::process::CommandExt as _;

    let (program, _) = command
        .split_first()
        .ok_or_else(|| setup_failure("queue-hold child command cannot be empty"))?;
    if program.is_empty() {
        return Err(setup_failure("queue-hold child program cannot be empty"));
    }
    let queue = Queue::new(state_dir).map_err(setup_io)?;
    let canonical_state = fs::canonicalize(queue.state_dir()).map_err(setup_io)?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| setup_failure("queue-hold timeout cannot be represented"))?;
    let drain_lock = loop {
        if let Some(lock) = queue.acquire_drain_lock().map_err(|error| {
            setup_failure(format!("could not acquire queue admission lock: {error}"))
        })? {
            break lock;
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(ExitCode::from(TIMEOUT_EXIT));
        }
        thread::sleep(RETRY.min(deadline - now));
    };
    let lock_file = fs::canonicalize(queue.lock_file()).map_err(setup_io)?;
    let mut ledger = LedgerLock::acquire(&canonical_state)?;
    let prior = ledger.read()?;
    refuse_contradictory_live_owner(prior.as_ref())?;
    let generation = prior
        .as_ref()
        .map_or(Ok(1), |record| record.generation.checked_add(1).ok_or(()))
        .map_err(|()| setup_failure("queue-hold generation overflow"))?;
    let hold_id = random_hold_id()?;
    let owner_pid = std::process::id();
    let owner_process_start = process_start_digest(owner_pid)?;
    let lock_fd = drain_lock
        .prepare_exec_inheritance(lock_identity_offset(&hold_id))
        .map_err(setup_io)?;
    let record = HoldRecord {
        schema: SCHEMA,
        kind: KIND.to_owned(),
        hold_id: hold_id.clone(),
        generation,
        purpose: scope.purpose.clone(),
        host_id: scope.host_id.clone(),
        services: scope.services.clone(),
        repos: scope.repos.clone(),
        runners: scope.runners.clone(),
        scope_digest: scope.digest.clone(),
        state_dir: canonical_state.clone(),
        lock_file,
        owner_pid,
        owner_process_start: owner_process_start.clone(),
        acquired_at: Utc::now().to_rfc3339(),
        status: HoldStatus::Held,
        revision: 1,
        revoked_at: None,
        revoked_reason: None,
    };
    ledger.write(&record)?;
    drop(ledger);
    let (program, args) = command
        .split_first()
        .expect("queue-hold command checked before lock acquisition");
    let error = Command::new(program)
        .args(args)
        .env("SHIPYARD_QUEUE_HOLD_SCHEMA", SCHEMA.to_string())
        .env("SHIPYARD_QUEUE_HOLD_ID", &hold_id)
        .env("SHIPYARD_QUEUE_HOLD_GENERATION", generation.to_string())
        .env("SHIPYARD_QUEUE_HOLD_PURPOSE", &scope.purpose)
        .env("SHIPYARD_QUEUE_HOLD_HOST_ID", &scope.host_id)
        .env("SHIPYARD_QUEUE_HOLD_SCOPE_DIGEST", &scope.digest)
        .env("SHIPYARD_QUEUE_HOLD_FD", lock_fd.to_string())
        .env("SHIPYARD_QUEUE_HOLD_STATE_DIR", &canonical_state)
        .env("SHIPYARD_QUEUE_HOLD_OWNER_PID", owner_pid.to_string())
        .env(
            "SHIPYARD_QUEUE_HOLD_OWNER_PROCESS_START",
            owner_process_start,
        )
        .exec();
    Err(setup_failure(format!(
        "could not exec queue-hold child: {error}"
    )))
}

#[cfg(not(target_os = "macos"))]
fn exec_hold(
    _state_dir: &Path,
    _scope: &Scope,
    _timeout: Duration,
    _command: &[OsString],
) -> Result<ExitCode, CliFailure> {
    Err(setup_failure("queue-hold is unsupported on this platform"))
}

#[allow(clippy::too_many_arguments)]
fn verify_hold(
    state_dir: &Path,
    hold_id: &str,
    generation: u64,
    host_id: &str,
    scope_digest: &str,
    owner_pid: u32,
    owner_process_start: &str,
    fd: i32,
) -> Result<VerifyResponse, CliFailure> {
    validate_value("hold id", hold_id)?;
    validate_value("host id", host_id)?;
    validate_digest(scope_digest)?;
    validate_digest(owner_process_start)?;
    if generation == 0 || owner_pid == 0 || fd < 0 {
        return Err(setup_failure(
            "queue-hold generation, owner PID, and FD must be positive",
        ));
    }
    let canonical_state = fs::canonicalize(state_dir).map_err(setup_io)?;
    let record = LedgerLock::acquire(&canonical_state)?.read_required()?;
    let refused = |reason| verify_response(&record, hold_id, generation, "refused", Some(reason));
    if record.hold_id != hold_id || record.generation != generation {
        return Ok(refused("stale_generation"));
    }
    if record.status == HoldStatus::Revoked {
        return Ok(refused("revoked"));
    }
    if record.status != HoldStatus::Held {
        return Ok(refused("owner_dead"));
    }
    if record.host_id != host_id || record.scope_digest != scope_digest {
        return Ok(refused("scope_mismatch"));
    }
    match process_id_liveness(record.owner_pid) {
        ProcessLiveness::Dead => return Ok(refused("owner_dead")),
        ProcessLiveness::Unknown => {
            return Err(setup_failure("queue-hold owner liveness is unknown"));
        }
        ProcessLiveness::Alive => {}
    }
    if record.owner_pid != owner_pid || record.owner_process_start != owner_process_start {
        return Ok(refused("owner_identity_mismatch"));
    }
    if !verifier_is_owner_or_descendant(owner_pid)? {
        return Ok(refused("owner_identity_mismatch"));
    }
    if process_start_digest(owner_pid)? != owner_process_start {
        return Ok(refused("owner_identity_mismatch"));
    }
    if !lock_fd_matches(&record, &canonical_state, fd)? {
        return Ok(refused("lock_fd_invalid"));
    }
    Ok(verify_response(&record, hold_id, generation, "held", None))
}

fn revoke_hold(
    state_dir: &Path,
    hold_id: &str,
    generation: u64,
    reason: &str,
) -> Result<RevokeResponse, CliFailure> {
    let canonical_state = fs::canonicalize(state_dir).map_err(setup_io)?;
    let mut ledger = LedgerLock::acquire(&canonical_state)?;
    let mut record = ledger.read_required()?;
    if record.hold_id != hold_id || record.generation != generation {
        return Ok(revoke_response(
            hold_id,
            generation,
            record.revision,
            "refused",
            Some("stale_generation"),
        ));
    }
    if record.status != HoldStatus::Held {
        let refusal = if record.status == HoldStatus::Revoked {
            "revoked"
        } else {
            "owner_dead"
        };
        return Ok(revoke_response(
            hold_id,
            generation,
            record.revision,
            "refused",
            Some(refusal),
        ));
    }
    match process_id_liveness(record.owner_pid) {
        ProcessLiveness::Dead => {
            return Ok(revoke_response(
                hold_id,
                generation,
                record.revision,
                "refused",
                Some("owner_dead"),
            ));
        }
        ProcessLiveness::Unknown => {
            return Err(setup_failure("queue-hold owner liveness is unknown"));
        }
        ProcessLiveness::Alive => {
            if process_start_digest(record.owner_pid)? != record.owner_process_start {
                return Ok(revoke_response(
                    hold_id,
                    generation,
                    record.revision,
                    "refused",
                    Some("owner_identity_mismatch"),
                ));
            }
        }
    }
    record.status = HoldStatus::Revoked;
    record.revoked_at = Some(Utc::now().to_rfc3339());
    record.revoked_reason = Some(reason.to_owned());
    record.revision = record
        .revision
        .checked_add(1)
        .ok_or_else(|| setup_failure("queue-hold revision overflow"))?;
    ledger.write(&record)?;
    Ok(revoke_response(
        hold_id,
        generation,
        record.revision,
        "revoked",
        None,
    ))
}

impl Scope {
    fn new(
        purpose: String,
        host_id: String,
        services: Vec<String>,
        repos: Vec<String>,
        runners: Vec<String>,
    ) -> Result<Self, CliFailure> {
        if purpose != PURPOSE {
            return Err(setup_failure(format!(
                "queue-hold purpose must be {PURPOSE}"
            )));
        }
        validate_value("host id", &host_id)?;
        let services = canonical_set("service", services, true)?;
        // Provider-only hosts still have exact services but may have no
        // repository-scoped persistent runner. Keep those identities optional
        // and bind every supplied value into the canonical scope digest.
        let repos = canonical_set("repository", repos, false)?;
        let runners = canonical_set("runner", runners, false)?;
        for repo in &repos {
            let mut parts = repo.split('/');
            if parts.next().is_none()
                || parts.next().is_none()
                || parts.next().is_some()
                || repo.starts_with('/')
                || repo.ends_with('/')
            {
                return Err(setup_failure(format!(
                    "queue-hold repository must be owner/name: {repo}"
                )));
            }
        }
        let digest = scope_digest(&purpose, &host_id, &services, &repos, &runners);
        Ok(Self {
            purpose,
            host_id,
            services,
            repos,
            runners,
            digest,
        })
    }
}

impl LedgerLock {
    fn acquire(state_dir: &Path) -> Result<Self, CliFailure> {
        crate::writer_domain_lease::ensure_protected_dir_all(state_dir).map_err(setup_io)?;
        let path = state_dir.join("queue-hold.ledger.lock");
        let creation =
            crate::writer_domain_lease::acquire_for_protected_creation(&path).map_err(setup_io)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(setup_io)?;
        drop(creation);
        file.lock_exclusive().map_err(setup_io)?;
        Ok(Self {
            state_dir: state_dir.to_owned(),
            file,
        })
    }

    fn path(&self) -> PathBuf {
        self.state_dir.join("queue-hold.json")
    }

    fn read(&self) -> Result<Option<HoldRecord>, CliFailure> {
        match fs::read(self.path()) {
            Ok(bytes) => {
                let record: HoldRecord = serde_json::from_slice(&bytes).map_err(|error| {
                    setup_failure(format!("queue-hold ledger is corrupt: {error}"))
                })?;
                validate_record(&record, &self.state_dir)?;
                Ok(Some(record))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(setup_io(error)),
        }
    }

    fn read_required(&self) -> Result<HoldRecord, CliFailure> {
        self.read()?
            .ok_or_else(|| setup_failure("queue-hold ledger is missing"))
    }

    fn write(&mut self, record: &HoldRecord) -> Result<(), CliFailure> {
        write_json_atomic(&self.path(), record).map_err(setup_io)
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn validate_record(record: &HoldRecord, state_dir: &Path) -> Result<(), CliFailure> {
    if record.schema != SCHEMA
        || record.kind != KIND
        || record.generation == 0
        || record.revision == 0
        || record.owner_pid == 0
        || record.purpose != PURPOSE
        || record.services.is_empty()
        || record.state_dir != state_dir
        || record.lock_file != state_dir.join("queue.lock")
    {
        return Err(setup_failure("queue-hold ledger authority is invalid"));
    }
    validate_value("hold id", &record.hold_id)?;
    validate_value("host id", &record.host_id)?;
    validate_digest(&record.scope_digest)?;
    validate_digest(&record.owner_process_start)?;
    let expected = scope_digest(
        &record.purpose,
        &record.host_id,
        &record.services,
        &record.repos,
        &record.runners,
    );
    if expected != record.scope_digest
        || !strictly_sorted(&record.services)
        || !strictly_sorted(&record.repos)
        || !strictly_sorted(&record.runners)
    {
        return Err(setup_failure("queue-hold ledger scope is invalid"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn refuse_contradictory_live_owner(prior: Option<&HoldRecord>) -> Result<(), CliFailure> {
    let Some(record) = prior.filter(|record| record.status == HoldStatus::Held) else {
        return Ok(());
    };
    match process_id_liveness(record.owner_pid) {
        ProcessLiveness::Dead => Ok(()),
        ProcessLiveness::Unknown => {
            Err(setup_failure("prior queue-hold owner liveness is unknown"))
        }
        ProcessLiveness::Alive => {
            if process_start_digest(record.owner_pid)? == record.owner_process_start {
                Err(setup_failure(
                    "prior queue-hold owner is live despite queue.lock acquisition",
                ))
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(unix)]
fn verifier_is_owner_or_descendant(owner_pid: u32) -> Result<bool, CliFailure> {
    Ok(owner_pid == std::process::id()
        || crate::worker_process_custody::process_is_descendant(owner_pid, std::process::id())
            .map_err(setup_io)?)
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn verifier_is_owner_or_descendant(_owner_pid: u32) -> Result<bool, CliFailure> {
    // Queue-hold exec is Unix-only. Never infer process-tree custody on a
    // platform where Shipyard has no exact descendant observation primitive.
    Ok(false)
}

#[allow(clippy::unnecessary_wraps)]
fn lock_fd_matches(record: &HoldRecord, state_dir: &Path, fd: i32) -> Result<bool, CliFailure> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt as _;

        #[allow(deprecated)]
        use nix::fcntl::{FlockArg, flock};

        if record.state_dir != state_dir || record.lock_file != state_dir.join("queue.lock") {
            return Ok(false);
        }
        let Ok(expected) = fs::metadata(&record.lock_file) else {
            return Ok(false);
        };
        let inherited_path = format!("/dev/fd/{fd}");
        let Ok(mut inherited) = File::open(inherited_path) else {
            return Ok(false);
        };
        let observed = inherited.metadata().map_err(setup_io)?;
        if expected.dev() != observed.dev() || expected.ino() != observed.ino() {
            return Ok(false);
        }
        if inherited.stream_position().map_err(setup_io)? != lock_identity_offset(&record.hold_id) {
            return Ok(false);
        }

        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&record.lock_file)
            .map_err(setup_io)?;
        match contender.try_lock_exclusive() {
            Ok(()) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(setup_io(error)),
        }
        #[allow(deprecated)]
        match flock(fd, FlockArg::LockExclusiveNonblock) {
            Ok(()) => Ok(true),
            Err(nix::errno::Errno::EWOULDBLOCK) => Ok(false),
            Err(error) => Err(setup_failure(format!(
                "queue-hold lock FD observation failed: {error}"
            ))),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (record, state_dir, fd);
        Ok(false)
    }
}

#[cfg(target_os = "macos")]
fn lock_identity_offset(hold_id: &str) -> u64 {
    let digest = Sha256Digest::of_bytes(hold_id.as_bytes());
    let prefix = &digest.as_str()[..16];
    let value = u64::from_str_radix(prefix, 16).expect("SHA-256 prefix is hexadecimal");
    (value & 0x3fff_ffff_ffff_ffff) | 0x4000_0000_0000_0000
}

#[cfg(unix)]
fn process_start_digest(pid: u32) -> Result<String, CliFailure> {
    crate::worker_process_custody::process_start_identity(pid)
        .map_err(setup_io)?
        .map(|identity| Sha256Digest::of_bytes(&identity).as_str().to_owned())
        .ok_or_else(|| setup_failure("queue-hold owner exited before birth identity capture"))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn process_start_digest(_pid: u32) -> Result<String, CliFailure> {
    Err(setup_failure(
        "queue-hold process birth identity is unsupported on this platform",
    ))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("queue-hold path has no parent"))?;
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    File::open(parent)?.sync_all()
}

fn canonical_set(
    label: &str,
    values: Vec<String>,
    required: bool,
) -> Result<Vec<String>, CliFailure> {
    let mut set = BTreeSet::new();
    for value in values {
        validate_value(label, &value)?;
        set.insert(value.trim().to_owned());
    }
    if required && set.is_empty() {
        return Err(setup_failure(format!(
            "queue-hold requires at least one {label}"
        )));
    }
    Ok(set.into_iter().collect())
}

fn validate_value(label: &str, value: &str) -> Result<(), CliFailure> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > 512
        || value.chars().any(char::is_control)
    {
        return Err(setup_failure(format!("queue-hold {label} is invalid")));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), CliFailure> {
    Sha256Digest::parse(value.to_owned())
        .map(|_| ())
        .map_err(|_| setup_failure("queue-hold digest is invalid"))
}

fn scope_digest(
    purpose: &str,
    host_id: &str,
    services: &[String],
    repos: &[String],
    runners: &[String],
) -> String {
    let bytes = serde_json::to_vec(&(PURPOSE, purpose, host_id, services, repos, runners))
        .expect("queue-hold canonical scope is serializable");
    Sha256Digest::of_bytes(&bytes).as_str().to_owned()
}

fn strictly_sorted(values: &[String]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
        && values
            .iter()
            .all(|value| validate_value("scope", value).is_ok())
}

#[cfg(target_os = "macos")]
fn random_hold_id() -> Result<String, CliFailure> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(setup_io)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

fn verify_response(
    record: &HoldRecord,
    hold_id: &str,
    generation: u64,
    status: &'static str,
    reason: Option<&'static str>,
) -> VerifyResponse {
    VerifyResponse {
        schema: SCHEMA,
        command: "queue-hold.verify",
        status,
        reason,
        hold_id: hold_id.to_owned(),
        generation,
        revision: record.revision,
        scope_digest: record.scope_digest.clone(),
    }
}

fn revoke_response(
    hold_id: &str,
    generation: u64,
    revision: u64,
    status: &'static str,
    reason: Option<&'static str>,
) -> RevokeResponse {
    RevokeResponse {
        schema: SCHEMA,
        command: "queue-hold.revoke",
        status,
        reason,
        hold_id: hold_id.to_owned(),
        generation,
        revision,
    }
}

fn render_verify<W: Write>(
    stdout: &mut W,
    json: bool,
    response: &VerifyResponse,
) -> Result<(), CliFailure> {
    if json {
        crate::output::write_pretty_json(stdout, response)
            .map_err(|error| setup_failure(error.to_string()))
    } else if response.status == "held" {
        writeln!(
            stdout,
            "queue hold {} generation {} is live",
            response.hold_id, response.generation
        )
        .map_err(setup_io)
    } else {
        writeln!(
            stdout,
            "queue hold {} generation {} refused: {}",
            response.hold_id,
            response.generation,
            response.reason.unwrap_or("unknown")
        )
        .map_err(setup_io)
    }
}

fn render_revoke<W: Write>(
    stdout: &mut W,
    json: bool,
    response: &RevokeResponse,
) -> Result<(), CliFailure> {
    if json {
        crate::output::write_pretty_json(stdout, response)
            .map_err(|error| setup_failure(error.to_string()))
    } else {
        writeln!(
            stdout,
            "queue hold {} generation {} {}{}",
            response.hold_id,
            response.generation,
            response.status,
            response
                .reason
                .map_or_else(String::new, |reason| format!(": {reason}"))
        )
        .map_err(setup_io)
    }
}

#[allow(clippy::needless_pass_by_value)]
fn setup_io(error: io::Error) -> CliFailure {
    setup_failure(format!("queue-hold I/O failed: {error}"))
}

fn setup_failure(message: impl Into<String>) -> CliFailure {
    CliFailure::new(SETUP_EXIT, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_sorted_deduplicated_and_rejects_blank_runner_identity() {
        let scope = Scope::new(
            PURPOSE.to_owned(),
            "m1".to_owned(),
            vec!["svc-b".to_owned(), "svc-a".to_owned(), "svc-a".to_owned()],
            vec!["owner/repo".to_owned()],
            vec!["runner-b".to_owned(), "runner-a".to_owned()],
        )
        .expect("scope");
        assert_eq!(scope.services, ["svc-a", "svc-b"]);
        assert_eq!(scope.runners, ["runner-a", "runner-b"]);
        assert_eq!(scope.digest.len(), 64);

        assert!(
            Scope::new(
                PURPOSE.to_owned(),
                "m1".to_owned(),
                vec!["svc".to_owned()],
                Vec::new(),
                vec![String::new()],
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn exact_revoke_is_revision_fenced_and_stale_generation_refuses() {
        let temp = tempfile::tempdir().expect("tempdir");
        let queue = Queue::new(temp.path()).expect("queue");
        let state_dir = fs::canonicalize(temp.path()).expect("canonical state");
        let lock_file = queue.lock_file();
        File::create(&lock_file).expect("lock file");
        let lock_file = fs::canonicalize(lock_file).expect("canonical lock file");
        let scope = Scope::new(
            PURPOSE.to_owned(),
            "m1".to_owned(),
            vec!["svc".to_owned()],
            Vec::new(),
            Vec::new(),
        )
        .expect("scope");
        let mut ledger = LedgerLock::acquire(&state_dir).expect("ledger");
        ledger
            .write(&HoldRecord {
                schema: SCHEMA,
                kind: KIND.to_owned(),
                hold_id: "hold-1".to_owned(),
                generation: 7,
                purpose: PURPOSE.to_owned(),
                host_id: scope.host_id,
                services: scope.services,
                repos: scope.repos,
                runners: scope.runners,
                scope_digest: scope.digest,
                state_dir: state_dir.clone(),
                lock_file,
                owner_pid: std::process::id(),
                owner_process_start: process_start_digest(std::process::id()).expect("identity"),
                acquired_at: Utc::now().to_rfc3339(),
                status: HoldStatus::Held,
                revision: 1,
                revoked_at: None,
                revoked_reason: None,
            })
            .expect("seed");
        drop(ledger);

        let stale = revoke_hold(&state_dir, "hold-1", 6, "test").expect("stale response");
        assert_eq!(stale.status, "refused");
        assert_eq!(stale.reason, Some("stale_generation"));
        let revoked = revoke_hold(&state_dir, "hold-1", 7, "test").expect("revoke");
        assert_eq!(revoked.status, "revoked");
        assert_eq!(revoked.revision, 2);
        let repeated = revoke_hold(&state_dir, "hold-1", 7, "test").expect("repeat");
        assert_eq!(repeated.reason, Some("revoked"));
    }

    #[cfg(not(unix))]
    #[test]
    fn unsupported_platform_refuses_process_birth_identity() {
        let error = process_start_digest(std::process::id()).expect_err("unsupported identity");
        assert_eq!(error.code, SETUP_EXIT);
        assert!(
            error
                .message
                .contains("process birth identity is unsupported on this platform")
        );
    }
}
