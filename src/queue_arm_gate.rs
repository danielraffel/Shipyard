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
//! An operator's `--arm` is not a bypass: it asks the guard for its own
//! operator override ([`OperatorOverride`]), which the guard honours only at an
//! interactive terminal, with a stated reason, outside any agent's environment,
//! and records with the operator and the reason. Everywhere else the
//! override's variables are stripped, so an ambient one cannot leak in.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ghapp_guards::MANAGED_GUARDS;

/// Asks the guard for the review rule alone.
pub const REVIEW_ONLY_ENV: &str = "SHIPYARD_QUEUE_ARM_REVIEW_ONLY";
/// Names the Shipyard path landing, for the arms ledger.
pub const CALLER_ENV: &str = "SHIPYARD_QUEUE_ARM_CALLER";
/// Where the guard appends its ledger lines.
pub const ARMS_LOG_ENV: &str = "SHIPYARD_QUEUE_ARMS_LOG";
/// The ledger's file name in Shipyard's state directory.
pub const ARMS_LEDGER: &str = "queue-arms.jsonl";
/// The guard's operator override for the review rule; `--arm` sets it.
pub const UNREVIEWED_OVERRIDE_ENV: &str = "GHAPP_ALLOW_UNREVIEWED_ARM";
/// Why the operator overrides; the guard refuses an override without one.
pub const UNREVIEWED_OVERRIDE_REASON_ENV: &str = "GHAPP_ALLOW_UNREVIEWED_ARM_REASON";
/// Who the operator is, for the guard's override audit.
pub const UNREVIEWED_OVERRIDE_OPERATOR_ENV: &str = "GHAPP_ALLOW_UNREVIEWED_ARM_OPERATOR";
/// Every refusal starts with this, as the guard's own lines do.
pub const REFUSAL_PREFIX: &str = "queue-arm-guard:";
/// The `PATH` `ghapp` runs its guards with.
const TRUSTED_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// An operator's `--arm`: a request for the guard's audited override, which
/// the guard alone decides whether to honour.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatorOverride {
    /// Who asked, e.g. `user@host`.
    pub operator: String,
    /// `--arm-reason`, when given.
    pub reason: Option<String>,
}

/// Put the override `requested` on `command` (a guard run or the landing
/// itself), or strip any the environment carries.
///
/// The guard reads standard input to tell an operator's terminal from a
/// daemon or an agent, so a requested override inherits it.
pub fn apply_override(command: &mut Command, requested: Option<&OperatorOverride>) {
    let Some(requested) = requested else {
        command
            .env_remove(UNREVIEWED_OVERRIDE_ENV)
            .env_remove(UNREVIEWED_OVERRIDE_REASON_ENV)
            .env_remove(UNREVIEWED_OVERRIDE_OPERATOR_ENV);
        return;
    };
    command
        .env(UNREVIEWED_OVERRIDE_ENV, "1")
        .env(UNREVIEWED_OVERRIDE_OPERATOR_ENV, &requested.operator)
        .stdin(std::process::Stdio::inherit());
    match requested.reason.as_deref() {
        Some(reason) => command.env(UNREVIEWED_OVERRIDE_REASON_ENV, reason),
        None => command.env_remove(UNREVIEWED_OVERRIDE_REASON_ENV),
    };
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
/// cannot land anything pass without running the guard. `operator_override`
/// is an operator's `--arm`: a request for the guard's own audited override.
///
/// # Errors
///
/// A message starting with [`REFUSAL_PREFIX`] when the guard refuses or
/// cannot run.
pub fn check(
    gh: &Command,
    args: &[String],
    caller: &str,
    operator_override: Option<&OperatorOverride>,
) -> Result<(), String> {
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
    let outcome = run_guard(
        gh,
        args,
        caller,
        operator_override,
        &ledger,
        guards_dir.as_deref(),
    );
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
    operator_override: Option<&OperatorOverride>,
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
    for (key, value) in gh.get_envs() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    if let Some(dir) = gh.get_current_dir() {
        command.current_dir(dir);
    }
    command
        .arg(bundle.path().join("queue-arm-guard"))
        .args(args)
        .env("PATH", trusted_path())
        .env("GHAPP_REAL_GH", gh.get_program())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env(REVIEW_ONLY_ENV, "1")
        .env(CALLER_ENV, caller)
        .env(ARMS_LOG_ENV, ledger)
        .env_remove(crate::merge_queue::INTERNAL_QUEUE_MUTATION_ENV)
        .env_remove("GHAPP_ALLOW_QUEUE_REARM");
    apply_override(&mut command, operator_override);
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
mod tests;
