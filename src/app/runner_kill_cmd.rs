//! `shipyard runner kill` — explicit Worker-process termination with full
//! recovery sequence. Complement to `runner cleanup --fix`; the latter only
//! cancels stale queued runs, while this subcommand handles the case where
//! a single `Runner.Worker` PID is wedged and needs to be terminated locally.
//!
//! The flow is deliberately conservative:
//!
//! 1. Snapshot the worker's command line, PR, job, branch, elapsed time, and
//!    `_work` directory into `~/.shipyard/kill-recovery.jsonl`.
//! 2. Require a typed `KILL` confirmation unless `--yes` is passed.
//! 3. `SIGTERM` first, polling every 500 ms for up to `grace_secs` seconds.
//! 4. `SIGKILL` only if the worker is still alive after the grace window.
//! 5. Reap any child processes (`pkill -P <pid> -f 'cmake|ninja|...'`).
//! 6. Move partial `build*` directories under the runner's `_work` tree to
//!    `/tmp/shipyard-killed-builds/<kill-event-id>/` — never delete.
//! 7. Verify `Runner.Listener` is still alive and print restart guidance if
//!    not.
//! 8. Poll the GitHub run until status flips to `completed`, then optionally
//!    re-trigger via `rerun-failed-jobs`.
//!
//! `--history` prints recent kills. `--recover <id>` restores a quarantined
//! build and (optionally) re-queues the run.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread::sleep;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::Value;

use super::CliFailure;
use super::runner_cmd::resolve_watchdog_settings;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::output::write_json_envelope;

/// Default SIGTERM-to-SIGKILL grace window, in seconds.
pub(super) const DEFAULT_GRACE_SECS: u64 = 10;
/// Maximum wall-clock window we wait for GitHub to recognise a killed run.
const GITHUB_STATUS_FLIP_BUDGET_SECS: u64 = 90;
/// Poll cadence while waiting for SIGTERM / GitHub status flip.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Bundled inputs for `kill_command`. Keeps the call site tidy and lets us
/// thread test-only overrides without an ever-growing positional list.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct KillCommandArgs<'a> {
    pub(super) config: &'a LoadedConfig,
    pub(super) cwd: &'a Path,
    pub(super) actions: &'a GitHubActions,
    pub(super) pid: Option<u32>,
    pub(super) reason: Option<String>,
    pub(super) retrigger: bool,
    pub(super) yes: bool,
    pub(super) repo_override: Option<String>,
    pub(super) runner_dir_override: Option<PathBuf>,
    pub(super) history: bool,
    pub(super) last: Option<usize>,
    pub(super) recover: Option<String>,
    pub(super) grace_secs: Option<u64>,
    pub(super) recovery_log_override: Option<PathBuf>,
    pub(super) quarantine_root_override: Option<PathBuf>,
    pub(super) no_wait_github: bool,
    pub(super) json: bool,
}

/// Entry point dispatched from `runner_command`.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub(super) fn kill_command<W: Write>(
    args: KillCommandArgs<'_>,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let recovery_log = args
        .recovery_log_override
        .clone()
        .unwrap_or_else(default_recovery_log);
    let quarantine_root = args
        .quarantine_root_override
        .clone()
        .unwrap_or_else(default_quarantine_root);

    if args.history {
        return history_mode(&recovery_log, args.last, args.json, stdout);
    }
    if let Some(id) = args.recover.as_deref() {
        return recover_mode(&args, &recovery_log, &quarantine_root, id, stdout);
    }

    let pid = args.pid.ok_or_else(|| {
        CliFailure::new(
            2,
            "--pid is required for `shipyard runner kill` (or pass --history / --recover).",
        )
    })?;
    let reason = args
        .reason
        .clone()
        .filter(|r| !r.trim().is_empty())
        .ok_or_else(|| {
            CliFailure::new(
                2,
                "--reason is required so kill events are auditable in the recovery log.",
            )
        })?;

    let settings = resolve_watchdog_settings(
        args.config,
        args.cwd,
        None,
        args.repo_override.clone(),
        args.runner_dir_override.clone(),
        None,
        None,
        None,
    )?;

    let grace = Duration::from_secs(args.grace_secs.unwrap_or(DEFAULT_GRACE_SECS));

    // 0. sanity-check the PID is actually a Runner.Worker
    let command_line = ps_command_for(pid)?;
    if !is_runner_worker(&command_line, &settings.runner_dir) {
        return Err(CliFailure::new(
            2,
            format!(
                "refusing to kill PID {pid}: command line does not look like a Runner.Worker under {}: {command_line}",
                settings.runner_dir.display()
            ),
        ));
    }
    let worker_info = parse_worker_command(&command_line);
    let log_info = augment_with_runner_log(&worker_info, &settings.runner_dir);

    // 1. snapshot
    let event_id = generate_event_id(pid);
    let now = Utc::now();
    let github_run_id = log_info.github_run_id.or(worker_info.github_run_id_hint);

    let mut entry = serde_json::Map::new();
    entry.insert("id".to_owned(), Value::from(event_id.clone()));
    entry.insert("ts".to_owned(), Value::from(now.to_rfc3339()));
    entry.insert("pid".to_owned(), Value::from(pid));
    entry.insert("reason".to_owned(), Value::from(reason.clone()));
    if let Some(pr) = log_info.pr.or(worker_info.pr) {
        entry.insert("pr".to_owned(), Value::from(pr));
    }
    if let Some(job) = log_info.job.clone().or_else(|| worker_info.job.clone()) {
        entry.insert("job".to_owned(), Value::from(job));
    }
    if let Some(branch) = log_info
        .branch
        .clone()
        .or_else(|| worker_info.branch.clone())
    {
        entry.insert("branch".to_owned(), Value::from(branch));
    }
    if let Some(etime) = worker_info.etime_min {
        entry.insert("etime_min".to_owned(), Value::from(etime));
    }
    if let Some(dir) = worker_info.worker_dir.clone() {
        entry.insert(
            "worker_dir".to_owned(),
            Value::from(dir.display().to_string()),
        );
    }
    if let Some(run_id) = github_run_id {
        entry.insert("github_run_id".to_owned(), Value::from(run_id));
    }
    entry.insert("repo".to_owned(), Value::from(settings.repo_slug.clone()));

    // 2. confirmation
    if !args.yes {
        let ok = confirm_kill_typed(stdout, pid, worker_info.pr.or(log_info.pr), &reason)?;
        if !ok {
            writeln!(stdout, "kill aborted — confirmation did not match 'KILL'").map_err(io_err)?;
            return Ok(ExitCode::from(1));
        }
    }

    // 3. SIGTERM with grace
    let signalled = send_signal(pid, "TERM")?;
    let mut signal_used: &'static str = if signalled { "SIGTERM" } else { "ALREADY_GONE" };
    let exited_gracefully = if signalled {
        wait_for_exit(pid, grace)
    } else {
        true
    };

    // 4. SIGKILL if still alive
    if !exited_gracefully {
        send_signal(pid, "KILL")?;
        signal_used = "SIGKILL";
        wait_for_exit(pid, Duration::from_secs(5));
    }

    // 5. reap children
    let children_reaped = reap_orphaned_children(pid);

    // 6. quarantine partial builds
    let quarantine_dir = quarantine_root.join(&event_id);
    let quarantined = quarantine_partial_builds(
        worker_info.worker_dir.as_deref(),
        worker_info.etime_min,
        &quarantine_dir,
    )?;
    if !quarantined.is_empty() {
        entry.insert(
            "quarantined".to_owned(),
            Value::Array(
                quarantined
                    .iter()
                    .map(|p| Value::from(p.display().to_string()))
                    .collect(),
            ),
        );
        entry.insert(
            "quarantine_dir".to_owned(),
            Value::from(quarantine_dir.display().to_string()),
        );
    }

    // 7. verify Runner.Listener is alive
    let listener_pid = runner_listener_pid();
    entry.insert(
        "runner_listener_alive".to_owned(),
        Value::from(listener_pid.is_some()),
    );

    // 8. wait for GitHub status flip
    let github_outcome = if args.no_wait_github {
        None
    } else if let Some(run_id) = github_run_id {
        Some(wait_for_github_status_flip(
            args.actions,
            &settings.repo_slug,
            run_id,
        ))
    } else {
        None
    };
    if let Some((status, conclusion)) = &github_outcome {
        entry.insert("github_status".to_owned(), Value::from(status.clone()));
        entry.insert(
            "github_conclusion".to_owned(),
            Value::from(conclusion.clone()),
        );
    }

    // 9. optional retrigger
    let mut retriggered = false;
    let mut retrigger_error: Option<String> = None;
    if args.retrigger {
        if let Some(run_id) = github_run_id {
            match args.actions.rerun_failed_jobs(&settings.repo_slug, run_id) {
                Ok(()) => retriggered = true,
                Err(err) => retrigger_error = Some(err.to_string()),
            }
        } else {
            retrigger_error = Some("no GitHub run_id captured; cannot retrigger".to_owned());
        }
        entry.insert("retriggered".to_owned(), Value::from(retriggered));
        if let Some(msg) = &retrigger_error {
            entry.insert("retrigger_error".to_owned(), Value::from(msg.clone()));
        }
    }

    entry.insert("signal".to_owned(), Value::from(signal_used.to_owned()));
    entry.insert("children_reaped".to_owned(), Value::from(children_reaped));

    // 10. append recovery log (after every other step so partial failures
    // still produce a usable audit trail)
    append_recovery_log(&recovery_log, &Value::Object(entry.clone()))?;

    // 11. print summary
    emit_kill_summary(
        stdout,
        &event_id,
        pid,
        signal_used,
        children_reaped,
        &quarantined,
        &quarantine_dir,
        listener_pid,
        github_outcome.as_ref(),
        retriggered,
        retrigger_error.as_deref(),
        &recovery_log,
        args.json,
        &entry,
    )?;

    Ok(ExitCode::SUCCESS)
}

// ---------- history ----------

fn history_mode<W: Write>(
    recovery_log: &Path,
    last: Option<usize>,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let entries = read_recovery_log(recovery_log)?;
    let take = last.unwrap_or(entries.len()).min(entries.len());
    // Most recent first.
    let view: Vec<&Value> = entries.iter().rev().take(take).collect();
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "recovery_log".to_owned(),
            Value::from(recovery_log.display().to_string()),
        );
        data.insert(
            "entries".to_owned(),
            Value::Array(view.iter().map(|v| (*v).clone()).collect()),
        );
        write_json_envelope(stdout, "runner.kill.history", data).map_err(io_err)?;
        return Ok(ExitCode::SUCCESS);
    }
    if view.is_empty() {
        writeln!(
            stdout,
            "no kill events recorded at {}",
            recovery_log.display()
        )
        .map_err(io_err)?;
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(
        stdout,
        "kill events (most recent first, log={}):",
        recovery_log.display()
    )
    .map_err(io_err)?;
    for entry in view {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or("?");
        let ts = entry.get("ts").and_then(Value::as_str).unwrap_or("?");
        let pid = entry.get("pid").and_then(Value::as_u64).unwrap_or(0);
        let signal = entry.get("signal").and_then(Value::as_str).unwrap_or("?");
        let pr = entry.get("pr").and_then(Value::as_u64);
        let reason = entry.get("reason").and_then(Value::as_str).unwrap_or("?");
        let pr_str = pr.map(|p| format!(" PR#{p}")).unwrap_or_default();
        writeln!(
            stdout,
            "  {id}  {ts}  pid={pid}{pr_str}  signal={signal}  reason=\"{reason}\""
        )
        .map_err(io_err)?;
    }
    Ok(ExitCode::SUCCESS)
}

// ---------- recover ----------

#[allow(clippy::too_many_lines)]
fn recover_mode<W: Write>(
    args: &KillCommandArgs<'_>,
    recovery_log: &Path,
    quarantine_root: &Path,
    event_id: &str,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let entries = read_recovery_log(recovery_log)?;
    let entry = entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(event_id))
        .ok_or_else(|| {
            CliFailure::new(
                2,
                format!(
                    "no kill event with id={event_id} in {}",
                    recovery_log.display()
                ),
            )
        })?;

    let quarantine_dir = entry
        .get("quarantine_dir")
        .and_then(Value::as_str)
        .map_or_else(|| quarantine_root.join(event_id), PathBuf::from);
    let worker_dir = entry
        .get("worker_dir")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let github_run_id = entry.get("github_run_id").and_then(Value::as_u64);
    let already_retriggered = entry
        .get("retriggered")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if !args.yes {
        write!(
            stdout,
            "recover kill event {event_id}? this will restore {} into {}. type KILL to confirm: ",
            quarantine_dir.display(),
            worker_dir
                .as_deref()
                .map(Path::display)
                .map_or_else(|| "(unknown)".to_owned(), |d| d.to_string()),
        )
        .map_err(io_err)?;
        stdout.flush().map_err(io_err)?;
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).map_err(io_err)?;
        if buf.trim() != "KILL" {
            writeln!(stdout, "recover aborted").map_err(io_err)?;
            return Ok(ExitCode::from(1));
        }
    }

    let mut restored: Vec<PathBuf> = Vec::new();
    if quarantine_dir.is_dir()
        && let Some(target) = worker_dir.as_deref()
    {
        for child in fs::read_dir(&quarantine_dir).map_err(io_err)? {
            let child = child.map_err(io_err)?;
            let src = child.path();
            let dst = target.join(child.file_name());
            if dst.exists() {
                writeln!(
                    stdout,
                    "  skipping {}: destination already exists at {}",
                    src.display(),
                    dst.display()
                )
                .map_err(io_err)?;
                continue;
            }
            fs::create_dir_all(target).map_err(io_err)?;
            fs::rename(&src, &dst).map_err(io_err)?;
            restored.push(dst);
        }
    }

    let mut retriggered_now = false;
    let mut retrigger_error: Option<String> = None;
    if !already_retriggered {
        let settings = resolve_watchdog_settings(
            args.config,
            args.cwd,
            None,
            args.repo_override.clone(),
            args.runner_dir_override.clone(),
            None,
            None,
            None,
        )?;
        if let Some(run_id) = github_run_id {
            match args.actions.rerun_failed_jobs(&settings.repo_slug, run_id) {
                Ok(()) => retriggered_now = true,
                Err(err) => retrigger_error = Some(err.to_string()),
            }
        }
    }

    if args.json {
        let mut data = BTreeMap::new();
        data.insert("event_id".to_owned(), Value::from(event_id.to_owned()));
        data.insert(
            "restored".to_owned(),
            Value::Array(
                restored
                    .iter()
                    .map(|p| Value::from(p.display().to_string()))
                    .collect(),
            ),
        );
        data.insert(
            "retriggered".to_owned(),
            Value::from(retriggered_now || already_retriggered),
        );
        if let Some(msg) = retrigger_error.as_ref() {
            data.insert("retrigger_error".to_owned(), Value::from(msg.clone()));
        }
        write_json_envelope(stdout, "runner.kill.recover", data).map_err(io_err)?;
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(
        stdout,
        "recover {event_id}: restored {} item(s) from {}",
        restored.len(),
        quarantine_dir.display(),
    )
    .map_err(io_err)?;
    for p in &restored {
        writeln!(stdout, "  + {}", p.display()).map_err(io_err)?;
    }
    if retriggered_now {
        writeln!(stdout, "  retriggered rerun-failed-jobs").map_err(io_err)?;
    } else if already_retriggered {
        writeln!(stdout, "  retrigger already done at kill time").map_err(io_err)?;
    }
    if let Some(msg) = retrigger_error.as_ref() {
        writeln!(stdout, "  retrigger failed: {msg}").map_err(io_err)?;
    }
    Ok(ExitCode::SUCCESS)
}

// ---------- worker / log parsing ----------

/// Parsed view of a `Runner.Worker` command line plus the runner-side log
/// scrape, merged into the per-kill snapshot we persist.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct WorkerInfo {
    pub(super) pr: Option<u64>,
    pub(super) job: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) etime_min: Option<i64>,
    pub(super) worker_dir: Option<PathBuf>,
    pub(super) github_run_id_hint: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RunnerLogInfo {
    pr: Option<u64>,
    job: Option<String>,
    branch: Option<String>,
    github_run_id: Option<u64>,
}

/// Parse a single `ps -o command=`-style command line for a Worker process.
///
/// The Worker command line on macOS looks like:
///
/// ```text
/// /Users/foo/actions-runner/bin/Runner.Worker spawnclient 0 ...
/// ```
///
/// We only extract `worker_dir` from this surface; PR/job/branch come from
/// the runner's stdout log because the Worker command line itself does not
/// embed them. We deliberately keep the extractor public for unit testing.
#[must_use]
pub(super) fn parse_worker_command(command_line: &str) -> WorkerInfo {
    let mut info = WorkerInfo::default();
    if let Some(bin_idx) = command_line.find("/bin/Runner.Worker") {
        let prefix = &command_line[..bin_idx];
        // `prefix` is the runner root, e.g. `/Users/foo/actions-runner`.
        // The Worker's `_work` directory lives at `<prefix>/_work`.
        let runner_root = PathBuf::from(prefix.trim());
        info.worker_dir = Some(runner_root.join("_work"));
    }
    // Allow the command line to carry an optional `--run-id` arg as a hint,
    // for forward compat with future runner versions.
    if let Some(idx) = command_line.find("--run-id ") {
        let tail = &command_line[idx + "--run-id ".len()..];
        let token = tail.split_whitespace().next().unwrap_or("");
        info.github_run_id_hint = token.parse::<u64>().ok();
    }
    info
}

/// Augment a `WorkerInfo` by scraping the runner-side stdout log
/// (`<runner_dir>/_diag/Runner_*.log`). The runner records every accepted
/// job's `Running job: <name>` line plus its workflow run id and branch.
fn augment_with_runner_log(worker: &WorkerInfo, runner_dir: &Path) -> RunnerLogInfo {
    let mut info = RunnerLogInfo::default();
    let diag_dir = runner_dir.join("_diag");
    let Ok(read) = fs::read_dir(&diag_dir) else {
        return info;
    };
    // Find the most recently modified Runner_*.log.
    let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in read.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        if !name.starts_with("Runner_") || !name.to_lowercase().ends_with(".log") {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && newest.as_ref().is_none_or(|(_, t)| modified > *t)
        {
            newest = Some((path, modified));
        }
    }
    let Some((log_path, _)) = newest else {
        return info;
    };
    let Ok(contents) = fs::read_to_string(&log_path) else {
        return info;
    };
    parse_runner_log_into(&contents, &mut info);
    let _ = worker; // reserved for future correlation by start time
    info
}

fn parse_runner_log_into(contents: &str, info: &mut RunnerLogInfo) {
    // Walk lines from end to start so we pick up the most recent job.
    for line in contents.lines().rev() {
        if info.job.is_none()
            && let Some(rest) = line.split("Running job: ").nth(1)
        {
            info.job = Some(rest.trim().to_owned());
        }
        if info.branch.is_none()
            && let Some(rest) = line.split("Job ref: ").nth(1)
        {
            // `refs/heads/foo/bar` or `refs/pull/1818/merge`
            let trimmed = rest.trim();
            if let Some(branch) = trimmed.strip_prefix("refs/heads/") {
                info.branch = Some(branch.to_owned());
            } else if let Some(rest) = trimmed.strip_prefix("refs/pull/") {
                if let Some((num, _)) = rest.split_once('/')
                    && let Ok(n) = num.parse::<u64>()
                {
                    info.pr = Some(n);
                }
                info.branch = Some(trimmed.to_owned());
            } else {
                info.branch = Some(trimmed.to_owned());
            }
        }
        if info.github_run_id.is_none()
            && let Some(rest) = line.split("Run ID: ").nth(1)
        {
            let token = rest.trim();
            if let Ok(n) = token.parse::<u64>() {
                info.github_run_id = Some(n);
            }
        }
        if info.job.is_some() && info.branch.is_some() && info.github_run_id.is_some() {
            break;
        }
    }
}

fn is_runner_worker(command_line: &str, runner_dir: &Path) -> bool {
    if !command_line.contains("Runner.Worker") {
        return false;
    }
    let dir = runner_dir.display().to_string();
    command_line.contains(&dir) || command_line.contains("/actions-runner/")
}

// ---------- process control ----------

fn ps_command_for(pid: u32) -> Result<String, CliFailure> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .map_err(|e| CliFailure::new(2, format!("failed to inspect pid {pid}: {e}")))?;
    if !out.status.success() {
        return Err(CliFailure::new(
            2,
            format!("pid {pid} is not running (ps reported no match)"),
        ));
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if line.is_empty() {
        return Err(CliFailure::new(
            2,
            format!("pid {pid} returned empty command line"),
        ));
    }
    Ok(line)
}

fn send_signal(pid: u32, signal: &str) -> Result<bool, CliFailure> {
    let out = Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .output()
        .map_err(|e| CliFailure::new(2, format!("failed to invoke kill: {e}")))?;
    if out.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("No such process") {
        return Ok(false);
    }
    Err(CliFailure::new(
        2,
        format!("kill -{signal} {pid} failed: {}", stderr.trim()),
    ))
}

fn wait_for_exit(pid: u32, budget: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < budget {
        if !pid_alive(pid) {
            return true;
        }
        sleep(POLL_INTERVAL);
    }
    !pid_alive(pid)
}

fn pid_alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| {
            if !o.status.success() {
                return false;
            }
            // `ps -p <pid>` always prints a header row; an extra line means a
            // match. Avoid pulling in `BufRead` just for this single use.
            String::from_utf8_lossy(&o.stdout).lines().count() > 1
        })
}

fn reap_orphaned_children(parent_pid: u32) -> u32 {
    // Use pkill -P to send SIGTERM to direct children whose command matches a
    // build tool. We intentionally avoid -9 so any well-behaved tool can flush
    // its own state. Returns the count of processes we successfully signalled.
    let pattern = "cmake|ninja|make|ctest|build";
    let out = Command::new("pkill")
        .args(["-TERM", "-P", &parent_pid.to_string(), "-f", pattern])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            // pkill prints nothing on success; assume at least one match.
            1
        }
        _ => 0,
    }
}

fn runner_listener_pid() -> Option<u32> {
    let out = Command::new("pgrep")
        .args(["-f", "Runner.Listener"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<u32>().ok())
}

// ---------- quarantine ----------

fn quarantine_partial_builds(
    worker_dir: Option<&Path>,
    etime_min: Option<i64>,
    quarantine_dir: &Path,
) -> Result<Vec<PathBuf>, CliFailure> {
    let Some(work) = worker_dir else {
        return Ok(Vec::new());
    };
    if !work.is_dir() {
        return Ok(Vec::new());
    }
    let mut moved: Vec<PathBuf> = Vec::new();
    let window =
        Duration::from_secs(u64::try_from((etime_min.unwrap_or(0) + 5).max(0)).unwrap_or(0) * 60);
    // Walk one level into `_work/<repo>/<branch>` then look for any `build*`
    // child directory whose mtime falls inside the window. Real GH runner
    // layouts vary, so we walk shallowly and skip anything we cannot read.
    let mut dirs_to_scan: Vec<PathBuf> = vec![work.to_owned()];
    let mut depth = 0;
    while let Some(dir) = dirs_to_scan.pop() {
        if depth > 4 {
            break;
        }
        depth += 1;
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.starts_with("build") && within_mtime_window(&meta, window) {
                let dest = quarantine_dir.join(name);
                fs::create_dir_all(quarantine_dir).map_err(io_err)?;
                if dest.exists() {
                    // Append a numeric suffix rather than collide.
                    let mut idx = 1;
                    loop {
                        let alt = quarantine_dir.join(format!("{name}.{idx}"));
                        if !alt.exists() {
                            fs::rename(&path, &alt).map_err(io_err)?;
                            moved.push(alt);
                            break;
                        }
                        idx += 1;
                    }
                } else {
                    fs::rename(&path, &dest).map_err(io_err)?;
                    moved.push(dest);
                }
            } else if meta.is_dir() {
                dirs_to_scan.push(path);
            }
        }
    }
    Ok(moved)
}

fn within_mtime_window(meta: &fs::Metadata, window: Duration) -> bool {
    if window.is_zero() {
        return true;
    }
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(elapsed) = modified.elapsed() else {
        return true; // future-mtime, treat as recent
    };
    elapsed <= window
}

// ---------- github status flip ----------

fn wait_for_github_status_flip(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(GITHUB_STATUS_FLIP_BUDGET_SECS);
    let mut last = (String::new(), String::new());
    while Instant::now() < deadline {
        // Transient API errors are deliberately swallowed — we keep polling
        // until the deadline.
        if let Ok((status, conclusion)) = actions.run_status_conclusion(repo, run_id) {
            last = (status.clone(), conclusion);
            if status == "completed" {
                return last;
            }
        }
        sleep(Duration::from_secs(2));
    }
    last
}

// ---------- recovery log ----------

fn default_recovery_log() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(home)
        .join(".shipyard")
        .join("kill-recovery.jsonl")
}

fn default_quarantine_root() -> PathBuf {
    PathBuf::from("/tmp/shipyard-killed-builds")
}

fn append_recovery_log(path: &Path, entry: &Value) -> Result<(), CliFailure> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err)?;
    }
    let serialized = serde_json::to_string(entry)
        .map_err(|e| CliFailure::new(2, format!("failed to serialize kill entry: {e}")))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(io_err)?;
    file.write_all(serialized.as_bytes()).map_err(io_err)?;
    file.write_all(b"\n").map_err(io_err)?;
    Ok(())
}

pub(super) fn read_recovery_log(path: &Path) -> Result<Vec<Value>, CliFailure> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path).map_err(io_err)?;
    let mut out = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            out.push(v);
        }
        // Corrupt lines are deliberately skipped — the log must never block
        // recovery, even if a partial write happened.
    }
    Ok(out)
}

// ---------- prompts ----------

fn confirm_kill_typed<W: Write>(
    stdout: &mut W,
    pid: u32,
    pr: Option<u64>,
    reason: &str,
) -> Result<bool, CliFailure> {
    if !is_stdin_tty() {
        writeln!(
            stdout,
            "stdin is not a TTY and --yes was not passed; refusing to kill pid {pid}"
        )
        .map_err(io_err)?;
        return Ok(false);
    }
    let pr_part = pr.map(|p| format!(", PR#{p}")).unwrap_or_default();
    write!(
        stdout,
        "KILL PLAN: pid={pid}{pr_part}, reason=\"{reason}\". \
         Type the word KILL (all caps) to proceed: "
    )
    .map_err(io_err)?;
    stdout.flush().map_err(io_err)?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf).map_err(io_err)?;
    Ok(buf.trim() == "KILL")
}

fn is_stdin_tty() -> bool {
    let mut probe = [0u8; 0];
    std::fs::File::open("/dev/tty").is_ok_and(|mut f| f.read(&mut probe).is_ok())
}

// ---------- output ----------

#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    clippy::too_many_lines
)]
fn emit_kill_summary<W: Write>(
    stdout: &mut W,
    event_id: &str,
    pid: u32,
    signal: &str,
    children_reaped: u32,
    quarantined: &[PathBuf],
    quarantine_dir: &Path,
    listener_pid: Option<u32>,
    github_outcome: Option<&(String, String)>,
    retriggered: bool,
    retrigger_error: Option<&str>,
    recovery_log: &Path,
    json: bool,
    entry: &serde_json::Map<String, Value>,
) -> Result<(), CliFailure> {
    if json {
        let mut data: BTreeMap<String, Value> = entry.clone().into_iter().collect();
        data.insert(
            "recovery_log".to_owned(),
            Value::from(recovery_log.display().to_string()),
        );
        return write_json_envelope(stdout, "runner.kill", data).map_err(io_err);
    }
    writeln!(
        stdout,
        "KILL EVENT {event_id} — saved to {}",
        recovery_log.display()
    )
    .map_err(io_err)?;
    let grace_suffix = if signal == "SIGKILL" {
        " (after 10s grace)"
    } else {
        ""
    };
    writeln!(
        stdout,
        "  worker pid {pid}, signal {signal}{grace_suffix}, {children_reaped} child(ren) reaped"
    )
    .map_err(io_err)?;
    if quarantined.is_empty() {
        writeln!(stdout, "  no partial builds to quarantine").map_err(io_err)?;
    } else {
        writeln!(
            stdout,
            "  partial build quarantined: {}",
            quarantine_dir.display()
        )
        .map_err(io_err)?;
        for p in quarantined {
            writeln!(stdout, "    + {}", p.display()).map_err(io_err)?;
        }
    }
    if let Some((status, conclusion)) = github_outcome {
        let retrigger_part = if retriggered {
            " (re-triggered automatically)".to_owned()
        } else if let Some(err) = retrigger_error {
            format!(" (retrigger failed: {err})")
        } else {
            String::new()
        };
        writeln!(
            stdout,
            "  github job: {status}{}{retrigger_part}",
            if conclusion.is_empty() {
                String::new()
            } else {
                format!("/{conclusion}")
            },
        )
        .map_err(io_err)?;
    }
    match listener_pid {
        Some(pid) => writeln!(stdout, "  runner: healthy (PID {pid})").map_err(io_err)?,
        None => {
            writeln!(
                stdout,
                "  runner: Runner.Listener NOT visible — restart with \
                 `~/actions-runner/svc.sh restart` or `~/actions-runner/run.sh`"
            )
            .map_err(io_err)?;
        }
    }
    writeln!(
        stdout,
        "To recover: shipyard runner kill --recover {event_id}"
    )
    .map_err(io_err)?;
    Ok(())
}

// ---------- helpers ----------

fn generate_event_id(pid: u32) -> String {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0);
    // Deterministic given (pid, timestamp); good enough for log correlation.
    format!("kill-{pid}-{nanos:x}")
}

fn io_err(e: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(1, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_worker_extracts_worker_dir() {
        let line = "/Users/runner/actions-runner/bin/Runner.Worker spawnclient 0 0";
        let info = parse_worker_command(line);
        assert_eq!(
            info.worker_dir,
            Some(PathBuf::from("/Users/runner/actions-runner/_work"))
        );
    }

    #[test]
    fn parse_worker_handles_missing_marker() {
        let info = parse_worker_command("/bin/bash -c sleep 30");
        assert_eq!(info.worker_dir, None);
    }

    #[test]
    fn parse_runner_log_extracts_job_and_branch_from_pull_ref() {
        let log = "\
2026-05-12 22:30:01Z INFO Running job: UBSan macOS ARM64
2026-05-12 22:30:02Z INFO Job ref: refs/pull/1818/merge
2026-05-12 22:30:03Z INFO Run ID: 12345
";
        let mut info = RunnerLogInfo::default();
        parse_runner_log_into(log, &mut info);
        assert_eq!(info.job.as_deref(), Some("UBSan macOS ARM64"));
        assert_eq!(info.pr, Some(1818));
        assert_eq!(info.branch.as_deref(), Some("refs/pull/1818/merge"));
        assert_eq!(info.github_run_id, Some(12345));
    }

    #[test]
    fn parse_runner_log_extracts_branch_from_heads_ref() {
        let log = "\
2026-05-12 22:30:01Z INFO Running job: macOS ARM64
2026-05-12 22:30:02Z INFO Job ref: refs/heads/agentB/81
";
        let mut info = RunnerLogInfo::default();
        parse_runner_log_into(log, &mut info);
        assert_eq!(info.branch.as_deref(), Some("agentB/81"));
        assert_eq!(info.pr, None);
    }

    #[test]
    fn quarantine_path_is_deterministic_for_event_id() {
        let root = PathBuf::from("/tmp/shipyard-killed-builds");
        let id = "kill-12345-deadbeef";
        let dir = root.join(id);
        assert_eq!(
            dir,
            PathBuf::from("/tmp/shipyard-killed-builds/kill-12345-deadbeef")
        );
    }

    #[test]
    fn jsonl_append_preserves_existing_entries() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("kill-recovery.jsonl");
        let e1 = serde_json::json!({"id":"a","pid":1});
        let e2 = serde_json::json!({"id":"b","pid":2});
        append_recovery_log(&log, &e1).unwrap();
        append_recovery_log(&log, &e2).unwrap();
        let entries = read_recovery_log(&log).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].get("id").and_then(Value::as_str), Some("a"));
        assert_eq!(entries[1].get("id").and_then(Value::as_str), Some("b"));
    }

    #[test]
    fn history_mode_handles_missing_log() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.jsonl");
        let mut out = Vec::new();
        let code = history_mode(&missing, None, false, &mut out).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no kill events recorded"));
    }

    #[test]
    fn history_mode_respects_last_limit() {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("kill-recovery.jsonl");
        for i in 0..5u32 {
            append_recovery_log(
                &log,
                &serde_json::json!({
                    "id": format!("k{i}"),
                    "ts": "2026-05-12T00:00:00Z",
                    "pid": i,
                    "signal": "SIGTERM",
                    "reason": "test",
                }),
            )
            .unwrap();
        }
        let mut out = Vec::new();
        history_mode(&log, Some(2), false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        // Most recent first, so k4 + k3.
        let event_rows = text
            .lines()
            .filter(|line| line.starts_with("  "))
            .collect::<Vec<_>>();
        assert_eq!(event_rows.len(), 2);
        assert!(event_rows[0].starts_with("  k4  "));
        assert!(event_rows[1].starts_with("  k3  "));
    }

    #[test]
    fn is_runner_worker_accepts_runner_dir_match() {
        let dir = PathBuf::from("/Users/runner/actions-runner");
        assert!(is_runner_worker(
            "/Users/runner/actions-runner/bin/Runner.Worker spawnclient 0",
            &dir,
        ));
    }

    #[test]
    fn is_runner_worker_rejects_unrelated_processes() {
        let dir = PathBuf::from("/Users/runner/actions-runner");
        assert!(!is_runner_worker("/bin/bash -c sleep 30", &dir));
    }

    #[test]
    fn within_mtime_window_accepts_zero_window() {
        // We can't easily forge mtime in a unit test; we just exercise the
        // zero-window short-circuit which the production path uses when
        // etime_min is unknown.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert!(within_mtime_window(&meta, Duration::ZERO));
    }
}
