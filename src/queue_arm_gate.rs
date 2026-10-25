//! Run the `ghapp` queue-arm guard's review rule before Shipyard lands a head.
//!
//! Shipyard's own arm, enqueue and merge requests carry
//! [`crate::merge_queue::INTERNAL_QUEUE_MUTATION_ENV`], so when `gh` is the
//! `ghapp` wrapper its queue-arm guard records them and steps aside, and when
//! `gh` is a native binary no guard runs at all. So that the guard's review
//! rule, not only [`crate::head_approval`] (its Rust twin), decides each of
//! Shipyard's own landings, [`check`] runs before any request that can arm,
//! enqueue or merge (`enablePullRequestAutoMerge`, `enqueuePullRequest`,
//! `mergePullRequest`, `gh pr merge`, REST `PUT pulls/<n>/merge`): it runs the
//! guard this build bundles ([`crate::ghapp_guards::MANAGED_GUARDS`], the same bytes
//! `shipyard guards install` places for `ghapp`) with
//! `SHIPYARD_QUEUE_ARM_REVIEW_ONLY=1`, against the same `gh` binary and token
//! the landing would use. The guard applies its review rule to the exact
//! request (an exact-head verdict from an allowed signer, bound by
//! `expectedHeadOid`, `--match-head-commit` or `sha=`), appends one line per
//! attempt, allowed or refused, to `queue-arms.jsonl` in Shipyard's state
//! directory, and exits non-zero to refuse. A guard that cannot run refuses.
//!
//! The review rule has no override: `--arm` only asks for an arm the verdict
//! allows, and any `GHAPP_ALLOW_*` variable is stripped from the guard run and
//! from the landing itself.
//!
//! ## Shipyard's authority is per call
//!
//! The internal marker and `SHIPYARD_QUEUE_ARM_REVIEW_ONLY` change what the
//! guards do, so neither is honoured on its own. For each call Shipyard builds
//! for one of its own queue mutations (and for each review-only guard run) it
//! issues a fresh random nonce ([`mark_internal_queue_mutation`]): the nonce
//! goes only into that child's environment ([`NONCE_ENV`], with
//! [`NONCE_DIR_ENV`]), and a 0600 record named by the SHA-256 of the nonce and
//! its purpose goes into a 0700 directory beside the arms ledger. The guard
//! honours the switch only when it can consume that record (the removal
//! guard's `consume_nonce`: owner, mode, age and purpose checked, then
//! unlinked), so each nonce works once, for the purpose it was issued for. A
//! marker without one, or with a wrong or reused one, is judged in full.
//!
//! The markers, the nonce variables and every `GHAPP_ALLOW_*` override are
//! never inherited: a Shipyard process that starts with any of them in its
//! environment re-executes itself without them
//! ([`relaunch_without_inherited_queue_authority`]), so no runner, validation
//! command or hook it spawns can carry them, and Shipyard sets them only on
//! the children it builds for a queue mutation.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use crate::ghapp_guards::MANAGED_GUARDS;

/// Asks the guard for the review rule alone.
pub const REVIEW_ONLY_ENV: &str = "SHIPYARD_QUEUE_ARM_REVIEW_ONLY";
/// Names the Shipyard path landing, for the arms ledger.
pub const CALLER_ENV: &str = "SHIPYARD_QUEUE_ARM_CALLER";
/// Where the guard appends its ledger lines.
pub const ARMS_LOG_ENV: &str = "SHIPYARD_QUEUE_ARMS_LOG";
/// The ledger's file name in Shipyard's state directory.
pub const ARMS_LEDGER: &str = "queue-arms.jsonl";
/// Carries this call's one-shot nonce.
pub const NONCE_ENV: &str = "SHIPYARD_QUEUE_MUTATION_NONCE";
/// Where this call's nonce record is.
pub const NONCE_DIR_ENV: &str = "SHIPYARD_QUEUE_MUTATION_NONCE_DIR";
/// The nonce records' directory, beside the arms ledger.
pub const NONCE_DIR_NAME: &str = "queue-mutation-nonces";
/// A record for the queue-arm guard's internal-marker check.
pub const NONCE_INTERNAL_ARM: &str = "internal-arm";
/// A record for the queue-removal guard's internal-marker check.
pub const NONCE_INTERNAL_REMOVAL: &str = "internal-removal";
/// A record for one review-only guard run.
pub const NONCE_REVIEW_ONLY: &str = "review-only";
/// How long an unconsumed record stays valid (the guards' `NONCE_MAX_AGE_SECONDS`).
const NONCE_MAX_AGE: Duration = Duration::from_mins(10);
/// Every guard override variable starts with this.
const GUARD_OVERRIDE_PREFIX: &str = "GHAPP_ALLOW_";
/// Every refusal starts with this, as the guard's own lines do.
pub const REFUSAL_PREFIX: &str = "queue-arm-guard:";
/// The `PATH` `ghapp` runs its guards with.
const TRUSTED_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Whether `name` is a variable that carries queue authority: the internal
/// marker, the review-only switch, a nonce variable or a guard override.
#[must_use]
pub fn is_queue_authority_variable(name: &str) -> bool {
    name == crate::merge_queue::INTERNAL_QUEUE_MUTATION_ENV
        || name == REVIEW_ONLY_ENV
        || name == NONCE_ENV
        || name == NONCE_DIR_ENV
        || name.starts_with(GUARD_OVERRIDE_PREFIX)
}

/// Remove from `command` every queue-authority variable it would inherit or
/// has been given.
pub fn strip_queue_authority(command: &mut Command) {
    let mut names = std::env::vars_os()
        .map(|(key, _)| key)
        .chain(command.get_envs().map(|(key, _)| key.to_os_string()))
        .filter(|key| key.to_str().is_some_and(is_queue_authority_variable))
        .collect::<Vec<_>>();
    names.extend(
        [
            crate::merge_queue::INTERNAL_QUEUE_MUTATION_ENV,
            REVIEW_ONLY_ENV,
            NONCE_ENV,
            NONCE_DIR_ENV,
        ]
        .map(OsString::from),
    );
    for name in names {
        command.env_remove(name);
    }
}

/// Mark `command` as Shipyard's own queue mutation, for this one call: the
/// internal marker and a fresh nonce whose records the arm and removal guards
/// each consume once. Any other queue authority it carried is stripped.
///
/// # Errors
///
/// When the nonce record cannot be written; the mutation must not run then.
pub fn mark_internal_queue_mutation(command: &mut Command) -> Result<(), String> {
    strip_queue_authority(command);
    let directory = nonce_dir(None);
    let nonce = issue_nonce(&directory, &[NONCE_INTERNAL_ARM, NONCE_INTERNAL_REMOVAL])?;
    command
        .env(crate::merge_queue::INTERNAL_QUEUE_MUTATION_ENV, "1")
        .env(NONCE_ENV, nonce)
        .env(NONCE_DIR_ENV, directory);
    Ok(())
}

/// The nonce directory: beside `ledger`, or beside the default ledger.
fn nonce_dir(ledger: Option<&Path>) -> PathBuf {
    #[cfg(test)]
    let ledger = ledger
        .map(Path::to_path_buf)
        .or_else(|| tests_support::current().map(|test| test.ledger));
    #[cfg(test)]
    let Some(ledger) = ledger else {
        return std::env::temp_dir().join(format!(
            "shipyard-test-{NONCE_DIR_NAME}-{}",
            std::process::id()
        ));
    };
    #[cfg(not(test))]
    let ledger = ledger.map_or_else(default_ledger, Path::to_path_buf);
    ledger
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        .join(NONCE_DIR_NAME)
}

/// The record's file name, as the guards compute it.
#[must_use]
pub fn nonce_record_name(nonce: &str, purpose: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(
        format!("{nonce}:{purpose}").as_bytes(),
    ))
}

/// Issue a fresh nonce with one record per purpose in `directory`, after
/// dropping records too old to be honoured. Returns the nonce.
///
/// # Errors
///
/// When the directory or a record cannot be written.
pub fn issue_nonce(directory: &Path, purposes: &[&str]) -> Result<String, String> {
    let failed = |what: &str, error: std::io::Error| {
        format!(
            "{REFUSAL_PREFIX} refusing the queue mutation: cannot {what} in {}: {error}",
            directory.display()
        )
    };
    create_private_dir(directory).map_err(|error| failed("create the nonce directory", error))?;
    prune_stale_records(directory);
    let nonce = hex::encode(random_bytes());
    for purpose in purposes {
        let path = directory.join(nonce_record_name(&nonce, purpose));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options
            .open(&path)
            .and_then(|mut file| file.write_all(purpose.as_bytes()))
            .map_err(|error| failed("write a nonce record", error))?;
    }
    Ok(nonce)
}

fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    if let Some(parent) = directory.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    fs::set_permissions(
        directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    Ok(())
}

fn prune_stale_records(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > NONCE_MAX_AGE);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// 32 bytes from the operating system's random source.
fn random_bytes() -> [u8; 32] {
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0_u8; 32];
    #[cfg(unix)]
    if fs::File::open("/dev/urandom")
        .and_then(|mut source| std::io::Read::read_exact(&mut source, &mut bytes))
        .is_ok()
    {
        return bytes;
    }
    // `RandomState` is keyed from the operating system's random source.
    for (index, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_usize(index);
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
        );
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes
}

/// Re-execute this process without any queue-authority variable it
/// inherited, so nothing it spawns inherits one either. `None` when there is
/// none to drop; otherwise the exit code to return (on Unix only when the
/// re-execution itself failed).
#[must_use]
pub fn relaunch_without_inherited_queue_authority() -> Option<ExitCode> {
    let mut command = relaunch_command(std::env::vars_os().map(|(key, _)| key))?;
    let failed = |error: std::io::Error| {
        let _ = crate::writer_domain_lease::write_stderr(format_args!(
            "shipyard: cannot drop inherited queue authority from the environment: {error}\n"
        ));
        ExitCode::from(1)
    };
    #[cfg(unix)]
    {
        let error = std::os::unix::process::CommandExt::exec(&mut command);
        Some(failed(error))
    }
    #[cfg(not(unix))]
    {
        Some(match command.status() {
            Ok(status) => {
                ExitCode::from(u8::try_from(status.code().unwrap_or(1).clamp(0, 255)).unwrap_or(1))
            }
            Err(error) => failed(error),
        })
    }
}

/// This executable with the same arguments and every queue-authority name
/// in `inherited` removed, or `None` when there is none.
#[must_use]
pub fn relaunch_command(inherited: impl Iterator<Item = OsString>) -> Option<Command> {
    let names = inherited
        .filter(|key| key.to_str().is_some_and(is_queue_authority_variable))
        .collect::<Vec<_>>();
    if names.is_empty() {
        return None;
    }
    let mut command = Command::new(
        std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from(std::env::args_os().next().unwrap_or_default())),
    );
    command.args(std::env::args_os().skip(1));
    for name in names {
        command.env_remove(name);
    }
    Some(command)
}

/// Whether `args` (a `gh` argv) can arm, enqueue or merge a pull request.
///
/// Shipyard builds these requests itself, so a plain substring test on the
/// argv is exact for them; anything it misses is not one of Shipyard's
/// landings.
#[must_use]
pub fn is_landing_request(args: &[String]) -> bool {
    if args.len() >= 2 && args[0] == "pr" && args[1] == "merge" {
        return !args.iter().any(|arg| arg == "--disable-auto");
    }
    if args.first().map(String::as_str) != Some("api") {
        return false;
    }
    let compact = args
        .iter()
        .flat_map(|arg| arg.chars())
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if [
        "enablepullrequestautomerge",
        "enqueuepullrequest",
        "mergepullrequest",
    ]
    .iter()
    .any(|mutation| compact.contains(mutation))
    {
        return true;
    }
    let explicit_get = args.windows(2).any(|pair| {
        matches!(pair[0].as_str(), "-X" | "--method") && pair[1].eq_ignore_ascii_case("GET")
    });
    !explicit_get && args.iter().any(|arg| is_rest_merge_endpoint(arg))
}

fn is_rest_merge_endpoint(arg: &str) -> bool {
    let parts = arg.trim_start_matches('/').split('/').collect::<Vec<_>>();
    matches!(parts.as_slice(), ["repos", owner, name, "pulls", number, "merge"]
        if !owner.is_empty() && !name.is_empty() && !number.is_empty()
            && number.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Whether `message` is a refusal from [`check`] or the guard.
#[must_use]
pub fn is_refusal(message: &str) -> bool {
    message.contains(REFUSAL_PREFIX)
}

/// Whether `message` is the guard refusing for want of a review (or because
/// it could not judge one), not a queue-state refusal.
#[must_use]
pub fn is_review_refusal(message: &str) -> bool {
    is_refusal(message) && message.contains("refusing to arm or merge")
}

/// Who is landing: the Shipyard subcommand running, e.g. `shipyard merge-steward run`.
#[must_use]
pub fn process_caller() -> String {
    let words = std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-') && !arg.contains(['/', '\\']))
        .take(2)
        .collect::<Vec<_>>();
    let mut caller = "shipyard".to_owned();
    for word in words {
        caller.push(' ');
        caller.push_str(&word);
    }
    format!("{caller} (pid {})", std::process::id())
}

/// Refuse `args` unless the queue-arm guard's review rule allows it.
///
/// `gh` is the prepared command the landing would run (its program, token and
/// working directory are reused); `args` is the landing's argv. Requests that
/// cannot land anything pass without running the guard.
///
/// # Errors
///
/// A message starting with [`REFUSAL_PREFIX`] when the guard refuses or
/// cannot run.
pub fn check(gh: &Command, args: &[String], caller: &str) -> Result<(), String> {
    if !is_landing_request(args) {
        return Ok(());
    }
    #[cfg(test)]
    let Some(test) = tests_support::current() else {
        return Ok(());
    };
    #[cfg(test)]
    let (ledger, guards_dir) = (test.ledger, Some(test.guards_dir));
    #[cfg(not(test))]
    let (ledger, guards_dir) = (default_ledger(), None::<PathBuf>);
    let outcome = run_guard(gh, args, caller, &ledger, guards_dir.as_deref());
    if let Err(error) = &outcome
        && !error.ran
    {
        record_unjudged(&ledger, args, caller, &error.message);
    }
    outcome.map_err(|error| error.message)
}

/// `queue-arms.jsonl` in Shipyard's state directory, or [`ARMS_LOG_ENV`].
#[must_use]
pub fn default_ledger() -> PathBuf {
    std::env::var_os(ARMS_LOG_ENV)
        .filter(|value| !value.is_empty())
        .map_or_else(
            || {
                crate::paths::RuntimePaths::current(crate::identity::RuntimeMode::Shipyard)
                    .state_dir
                    .join(ARMS_LEDGER)
            },
            PathBuf::from,
        )
}

struct GateError {
    message: String,
    /// Whether the guard ran (and so wrote its own ledger line).
    ran: bool,
}

fn run_guard(
    gh: &Command,
    args: &[String],
    caller: &str,
    ledger: &Path,
    guards_dir: Option<&Path>,
) -> Result<(), GateError> {
    let unavailable = |detail: String| GateError {
        message: format!(
            "{REFUSAL_PREFIX} refusing to arm or merge: the review guard could not run ({detail})"
        ),
        ran: false,
    };
    let bundle = tempfile::Builder::new()
        .prefix("shipyard-queue-arm-guard-")
        .tempdir()
        .map_err(|error| unavailable(format!("temporary directory: {error}")))?;
    // The arm guard loads its request parser from the removal guard beside it.
    for guard in MANAGED_GUARDS
        .iter()
        .filter(|guard| matches!(guard.name, "queue-removal-guard" | "queue-arm-guard"))
    {
        fs::write(bundle.path().join(guard.name), guard.contents)
            .map_err(|error| unavailable(format!("write {}: {error}", guard.name)))?;
    }
    let mut command = Command::new("python3");
    review_only_environment(gh, &mut command, ledger).map_err(|error| GateError {
        message: error,
        ran: false,
    })?;
    command
        .arg(bundle.path().join("queue-arm-guard"))
        .args(args)
        .env("PATH", trusted_path())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env(CALLER_ENV, caller)
        .env(ARMS_LOG_ENV, ledger);
    if let Some(dir) = guards_dir {
        command.env("SHIPYARD_GHAPP_GUARDS_DIR", dir);
    }
    let output = command
        .output()
        .map_err(|error| unavailable(format!("python3: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let detail = lines[lines.len().saturating_sub(6)..].join("\n");
    let judged = detail.contains(REFUSAL_PREFIX);
    Err(GateError {
        message: if judged {
            detail
        } else {
            format!(
                "{REFUSAL_PREFIX} refusing to arm or merge: the review guard exited {}: {detail}",
                output.status
            )
        },
        // A crash before the guard judged anything leaves no ledger line.
        ran: judged,
    })
}

/// The review-only guard run's environment and working directory: the
/// landing's own (`gh`'s token and directory), less any queue authority it
/// carries, which is not this run's (the landing's marker and its nonce
/// included), plus the review-only switch and a nonce of its own.
fn review_only_environment(
    gh: &Command,
    command: &mut Command,
    ledger: &Path,
) -> Result<(), String> {
    for (key, value) in gh.get_envs() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    if let Some(dir) = gh.get_current_dir() {
        command.current_dir(dir);
    }
    strip_queue_authority(command);
    let nonce_directory = nonce_dir(Some(ledger));
    let nonce = issue_nonce(&nonce_directory, &[NONCE_REVIEW_ONLY])?;
    command
        .env("GHAPP_REAL_GH", gh.get_program())
        .env(REVIEW_ONLY_ENV, "1")
        .env(NONCE_ENV, nonce)
        .env(NONCE_DIR_ENV, &nonce_directory);
    Ok(())
}

fn trusted_path() -> OsString {
    OsString::from(TRUSTED_PATH)
}

/// The ledger line for an attempt the guard never judged.
fn record_unjudged(ledger: &Path, args: &[String], caller: &str, detail: &str) {
    let line = serde_json::json!({
        "at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "caller": caller,
        "argv": args,
        "verb": "unknown",
        "verdict": "refused",
        "review": detail,
    });
    let written = ledger
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(ledger)
        })
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(error) = written {
        let _ = crate::writer_domain_lease::write_stderr(format_args!(
            "warning: queue-arms ledger {} not written: {error}\n",
            ledger.display()
        ));
    }
}

/// Test builds run no guard unless a test asks for the real one, so the many
/// tests whose fake `gh` serves only Shipyard's own reads keep working.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    #[derive(Clone)]
    pub(crate) struct RealGuard {
        pub(crate) ledger: PathBuf,
        pub(crate) guards_dir: PathBuf,
    }

    thread_local! {
        static REAL: RefCell<Option<RealGuard>> = const { RefCell::new(None) };
    }

    pub(super) fn current() -> Option<RealGuard> {
        REAL.with(|real| real.borrow().clone())
    }

    /// Run `body` with the bundled guard live on this thread, writing its
    /// ledger to `ledger` and reading signers and switches from `guards_dir`.
    /// The live-guard tests need a Unix `gh` script, so only they call it.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn with_real_guard<T>(
        ledger: &Path,
        guards_dir: &Path,
        body: impl FnOnce() -> T,
    ) -> T {
        let previous = REAL.with(|real| {
            real.replace(Some(RealGuard {
                ledger: ledger.to_path_buf(),
                guards_dir: guards_dir.to_path_buf(),
            }))
        });
        let result = body();
        REAL.with(|real| *real.borrow_mut() = previous);
        result
    }
}

#[cfg(test)]
mod portable_tests;
#[cfg(test)]
mod tests;
