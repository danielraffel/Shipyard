//! `shipyard runner` subcommand — health check, stale-queue cleanup, watch
//! daemon for the self-hosted GitHub Actions runner.
//!
//! Ports the Pulp planning watchdog prototype
//! (`pulp-planning/scripts/runner-watchdog.sh`, commit c719482) into a
//! first-class Shipyard subcommand. The pure detection logic lives in
//! `crate::runner_watchdog`; this module is the thin shell that talks to
//! `gh` and the local `ps` table.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread::sleep;
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;

use super::CliFailure;
use super::cli::RunnerCommand;
use super::fleet_status_cmd::fleet_liveness_policy;
use crate::cloud::{GitHubActions, QueuedRun};
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::runner_watchdog::{
    DEFAULT_MAX_JOB_MIN, DEFAULT_MAX_QUEUE_AGE_HOURS, DEFAULT_REAP_IN_PROGRESS_MAX_MIN,
    DEFAULT_REAP_QUEUED_MAX_MIN, DEFAULT_WATCH_INTERVAL_SECONDS, ReaperThresholds, RunnerHealth,
    RunnerReport, RunnerSnapshot, StaleQueuedRun, StaleRun, Symptom, WatchdogThresholds,
    assess_runner, compute_stale_queued_runs, compute_stale_runs, report_to_json,
};

mod watch;

use watch::dispatch_watch;
#[cfg(test)]
use watch::{fleet_liveness_due, resolve_reaper_thresholds, watch_exit_code};

const QUEUED_RUNS_LIMIT: u32 = 100;
/// Cap on the number of paginated `gh api` calls per reaper tick, per status.
/// Each page is 100 items, so the worst case is 500 `in_progress` + 500
/// `queued` runs scanned — far beyond any healthy repo. The paginated listers
/// stop early on the first short page, so a small repo still costs one call
/// per status.
const REAP_RUNS_MAX_PAGES: u32 = 5;

/// Entry point dispatched from `src/app.rs`.
#[allow(clippy::too_many_lines)]
pub(super) fn runner_command<W: Write>(
    command: RunnerCommand,
    config: &LoadedConfig,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let state_dir = &runtime_paths.state_dir;
    let actions = GitHubActions::from_loaded_config(cwd, config);
    match command {
        RunnerCommand::Status {
            runner_id,
            repo,
            runner_dir,
            max_job_min,
            max_queue_age_hours,
        } => status_command(
            config,
            cwd,
            &actions,
            runner_id,
            repo,
            runner_dir,
            max_job_min,
            max_queue_age_hours,
            json,
            stdout,
        ),
        RunnerCommand::Cleanup {
            dry_run,
            fix,
            stale_hours,
            repo,
            force_kill,
            yes,
        } => cleanup_command(
            config,
            cwd,
            &actions,
            dry_run,
            fix,
            stale_hours,
            repo,
            force_kill,
            yes,
            json,
            stdout,
        ),
        command @ RunnerCommand::Watch { .. } => {
            dispatch_watch(command, config, cwd, state_dir, &actions, json, stdout)
        }
        RunnerCommand::Kill {
            pid,
            reason,
            retrigger,
            yes,
            repo,
            runner_dir,
            history,
            last,
            recover,
            grace_secs,
            recovery_log,
            quarantine_root,
            no_wait_github,
        } => super::runner_kill_cmd::kill_command(
            super::runner_kill_cmd::KillCommandArgs {
                config,
                cwd,
                actions: &actions,
                pid,
                reason,
                retrigger,
                yes,
                repo_override: repo,
                runner_dir_override: runner_dir,
                history,
                last,
                recover,
                grace_secs,
                recovery_log_override: recovery_log,
                quarantine_root_override: quarantine_root,
                no_wait_github,
                json,
            },
            stdout,
        ),
        RunnerCommand::Tag { set } => {
            super::runner_provision_cmd::tag_command(state_dir, set, json, stdout)
        }
        RunnerCommand::Register {
            repo,
            count,
            machine_tag,
            labels,
            ci_root,
            dry_run,
        } => super::runner_provision_cmd::register_command(
            super::runner_provision_cmd::RegisterArgs {
                cwd,
                state_dir,
                actions: &actions,
                repo,
                count,
                machine_tag,
                labels,
                ci_root,
                dry_run,
                json,
            },
            stdout,
        ),
        RunnerCommand::List { repo, all_repos } => {
            super::runner_provision_cmd::list_command(cwd, &actions, &repo, all_repos, json, stdout)
        }
        RunnerCommand::Audit { repo } => {
            super::runner_provision_cmd::audit_command(cwd, &actions, &repo, json, stdout)
        }
        RunnerCommand::Capacity => super::capacity_cmd::capacity_command(config, json, stdout),
        RunnerCommand::FleetStatus {
            repo,
            base,
            target,
            queued_age_threshold_secs,
            queue_run_limit,
            merge_queue_stall_threshold_secs,
            release_stale_threshold_secs,
        } => super::fleet_status_cmd::fleet_status_command(
            super::fleet_status_cmd::FleetStatusArgs {
                repo,
                base,
                target,
                queued_age_threshold_secs,
                queue_run_limit,
                merge_queue_stall_threshold_secs,
                release_stale_threshold_secs,
            },
            config,
            cwd,
            state_dir,
            &actions,
            json,
            stdout,
        ),
        RunnerCommand::LocalLinuxLease {
            repo,
            profile,
            profile_file,
            context,
            lane,
            apply,
            watch,
            interval_secs,
            max_ticks,
        } => super::local_linux_lease_cmd::local_linux_lease_command(
            super::local_linux_lease_cmd::LocalLinuxLeaseArgs {
                repo,
                profile,
                profile_file,
                context,
                lane,
                apply,
                watch,
                interval_secs,
                max_ticks,
            },
            cwd,
            &actions,
            json,
            stdout,
        ),
        RunnerCommand::StewardHandoff {
            repo,
            pr,
            head,
            workstream_id,
            context_url,
            apply,
        } => super::merge_steward_cmd::steward_handoff_command(
            &super::merge_steward_cmd::StewardHandoffArgs {
                repo,
                pr,
                head,
                workstream_id,
                context_url,
                apply,
            },
            cwd,
            &actions,
            json,
            stdout,
        ),
        RunnerCommand::Steward {
            repo,
            base,
            opt_out_label,
            max_transient_reruns,
            no_coalesce,
            no_preempt_capacity,
            max_preemptions_per_head,
            apply,
            ledger,
        } => super::merge_steward_cmd::steward_command(
            &super::merge_steward_cmd::StewardCommandArgs {
                repos: repo,
                base,
                opt_out_label,
                managed_label: super::merge_steward_cmd::MANAGED_LABEL.to_owned(),
                handoff_context: super::merge_steward_cmd::HANDOFF_CONTEXT.to_owned(),
                max_transient_reruns,
                coalesce: !no_coalesce,
                preempt_capacity: !no_preempt_capacity,
                max_preemptions_per_head,
                apply,
                ledger,
            },
            cwd,
            mode,
            runtime_paths,
            &actions,
            json,
            stdout,
        ),
        RunnerCommand::RerouteWatch {
            repo,
            target,
            interval,
            flap_window,
            once,
            max_ticks,
            apply,
        } => super::reroute_cmd::reroute_watch_command(
            &super::reroute_cmd::RerouteWatchArgs {
                repo,
                target,
                interval_secs: interval,
                flap_window_secs: flap_window,
                once,
                max_ticks,
                apply,
            },
            config,
            cwd,
            &actions,
            json,
            stdout,
        ),
        RunnerCommand::Remove {
            name,
            repo,
            purge_dir,
            yes,
        } => super::runner_provision_cmd::remove_command(
            cwd, &actions, name, repo, purge_dir, yes, json, stdout,
        ),
    }
}

// ---------- status ----------

#[allow(clippy::too_many_arguments)]
fn status_command<W: Write>(
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    runner_id_override: Option<u64>,
    repo_override: Option<String>,
    runner_dir_override: Option<PathBuf>,
    max_job_min_override: Option<i64>,
    max_queue_age_hours_override: Option<i64>,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let settings = resolve_watchdog_settings(
        config,
        cwd,
        runner_id_override,
        repo_override,
        runner_dir_override,
        max_job_min_override,
        max_queue_age_hours_override,
        None,
    )?;
    let snapshot = fetch_runner_snapshot(actions, &settings)?;
    let queued_runs = fetch_queued_runs(actions, &settings.repo_slug)?;
    let report = assess_runner(&snapshot, &queued_runs, settings.thresholds, Utc::now());

    emit_status_report(stdout, &report, json)?;
    Ok(ExitCode::from(report.health.exit_code()))
}

fn emit_status_report<W: Write>(
    stdout: &mut W,
    report: &RunnerReport,
    json: bool,
) -> Result<(), CliFailure> {
    if json {
        let data = report_to_json(report);
        return write_json_envelope(stdout, "runner.status", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }

    let writes = (|| -> std::io::Result<()> {
        writeln!(
            stdout,
            "runner: {} (busy={}, workers={})",
            report.status, report.busy, report.worker_count
        )?;
        match report.health {
            RunnerHealth::Healthy => {
                writeln!(stdout, "OK: no symptoms detected")?;
            }
            RunnerHealth::Offline => {
                writeln!(
                    stdout,
                    "ERR: runner is not online; investigate before trusting CI."
                )?;
                for symptom in &report.symptoms {
                    writeln!(stdout, "  - {}", format_symptom_human(symptom))?;
                }
            }
            RunnerHealth::Stuck => {
                writeln!(stdout, "WARN: stuck-state symptoms detected:")?;
                for symptom in &report.symptoms {
                    writeln!(stdout, "  - {}", format_symptom_human(symptom))?;
                }
                if !report.stale_queued_runs.is_empty() {
                    writeln!(stdout, "stale queued runs:")?;
                    for run in &report.stale_queued_runs {
                        writeln!(
                            stdout,
                            "  - run {} ({}, branch={}) queued for {}s [{}]",
                            run.run_id,
                            run.workflow,
                            run.branch,
                            run.queued_for_secs,
                            if run.cancellation_safe {
                                "cancellable"
                            } else {
                                "protected: not cancellable"
                            },
                        )?;
                    }
                    if report
                        .stale_queued_runs
                        .iter()
                        .any(|run| run.cancellation_safe)
                    {
                        writeln!(stdout, "fix with: shipyard runner cleanup --fix")?;
                    }
                }
            }
        }
        Ok(())
    })();
    writes.map_err(|error| CliFailure::new(1, error.to_string()))
}

fn format_symptom_human(symptom: &Symptom) -> String {
    match symptom {
        Symptom::OfflineBusy => {
            "offline_busy: GitHub reports busy while the runner is offline; reconcile local VM/lease/job ownership before recovery".to_owned()
        }
        Symptom::OrphanedBusy => {
            "orphaned_busy: runner.busy=true but no Runner.Worker process visible (usually clears in 1-5 min)".to_owned()
        }
        Symptom::HungWorker {
            worker_age_min,
            threshold_min,
        } => format!(
            "hung_worker: Runner.Worker has been running {worker_age_min} min (> {threshold_min} min threshold)"
        ),
        Symptom::StaleQueuedRuns { count } => {
            format!("stale_queued_runs: {count} run(s) older than the queue-age cutoff")
        }
    }
}

// ---------- cleanup ----------

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn cleanup_command<W: Write>(
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    dry_run: bool,
    fix: bool,
    stale_hours_override: Option<i64>,
    repo_override: Option<String>,
    force_kill: bool,
    yes: bool,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let settings = resolve_watchdog_settings(
        config,
        cwd,
        None,
        repo_override,
        None,
        None,
        stale_hours_override,
        None,
    )?;
    let now = Utc::now();
    let queued_runs = fetch_queued_runs(actions, &settings.repo_slug)?;
    let stale = compute_stale_queued_runs(
        &queued_runs,
        settings.thresholds.max_queue_age_hours * 3_600,
        now,
    );

    // `--fix` takes precedence over the default-true `--dry-run`.
    let apply = fix && !dry_run_overridden_only(dry_run, fix);
    let mut cancelled = Vec::new();
    let mut failed = Vec::new();
    if apply {
        for run in stale.iter().filter(|run| run.cancellation_safe) {
            match actions.cancel_workflow_run(&settings.repo_slug, run.run_id) {
                Ok(()) => cancelled.push(run.run_id),
                Err(err) => failed.push((run.run_id, err.to_string())),
            }
        }
    }

    if force_kill {
        if !apply {
            return Err(CliFailure::new(
                1,
                "--force-kill requires --fix to acknowledge intent",
            ));
        }
        let confirmed = confirm_force_kill(yes, stdout)?;
        if confirmed {
            // We intentionally do not implement Worker-process termination
            // here. The prototype's lessons-learned section explicitly warned
            // that auto-kill is too risky to wire silently. The CLI prints a
            // diagnostic hint and exits without touching the local process
            // table.
            writeln!(
                stdout,
                "force-kill confirmed: refusing to terminate Runner.Worker automatically; \
                 inspect with `ps -ef | grep Runner.Worker` and kill manually if it is \
                 truly hung.",
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }

    emit_cleanup_report(stdout, &settings, &stale, &cancelled, &failed, apply, json)?;
    if !failed.is_empty() {
        return Ok(ExitCode::from(1));
    }
    if stale.is_empty() || apply {
        Ok(ExitCode::SUCCESS)
    } else {
        // Found stale runs but did not fix; communicate via exit 1 just like
        // the prototype script.
        Ok(ExitCode::from(1))
    }
}

// `--dry-run` defaults to true in clap; `--fix` is the explicit opt-in. The
// two flags are not declared as a conflict pair (so `shipyard runner cleanup
// --fix` works without needing to also pass `--no-dry-run`), so we only honour
// dry-run when --fix is not present.
fn dry_run_overridden_only(_dry_run: bool, fix: bool) -> bool {
    !fix
}

fn confirm_force_kill<W: Write>(yes: bool, stdout: &mut W) -> Result<bool, CliFailure> {
    if yes {
        return Ok(true);
    }
    if !is_stdin_tty() {
        writeln!(
            stdout,
            "--force-kill ignored: stdin is not a TTY and --yes was not passed",
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(false);
    }
    let first = prompt_line(
        stdout,
        "Force-kill the oldest Runner.Worker process? This may corrupt in-flight artifacts. [y/N] ",
    )?;
    if !first.eq_ignore_ascii_case("y") && !first.eq_ignore_ascii_case("yes") {
        return Ok(false);
    }
    let second = prompt_line(stdout, "Are you sure? Type the word KILL to confirm: ")?;
    Ok(second == "KILL")
}

fn prompt_line<W: Write>(stdout: &mut W, prompt: &str) -> Result<String, CliFailure> {
    write!(stdout, "{prompt}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    stdout
        .flush()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(buf.trim().to_owned())
}

fn is_stdin_tty() -> bool {
    // Best-effort TTY check without pulling in another crate. `read` on
    // closed stdin would block; we instead look at whether /dev/tty exists
    // and is readable from the controlling process. This is conservative —
    // when in doubt, treat stdin as non-TTY.
    let mut probe = [0u8; 0];
    std::fs::File::open("/dev/tty").is_ok_and(|mut f| f.read(&mut probe).is_ok())
}

fn emit_cleanup_report<W: Write>(
    stdout: &mut W,
    settings: &WatchdogSettings,
    stale: &[StaleQueuedRun],
    cancelled: &[u64],
    failed: &[(u64, String)],
    apply: bool,
    json: bool,
) -> Result<(), CliFailure> {
    let protected_run_ids = stale
        .iter()
        .filter(|run| !run.cancellation_safe)
        .map(|run| run.run_id)
        .collect::<Vec<_>>();
    let eligible_count = stale.len().saturating_sub(protected_run_ids.len());
    if json {
        return emit_cleanup_json(
            stdout,
            settings,
            stale,
            cancelled,
            failed,
            &protected_run_ids,
            apply,
        );
    }

    let result: std::io::Result<()> = (|| {
        if stale.is_empty() {
            writeln!(
                stdout,
                "No queued runs older than {}h on {}.",
                settings.thresholds.max_queue_age_hours, settings.repo_slug
            )?;
            return Ok(());
        }
        writeln!(
            stdout,
            "Found {} stale queued run(s) on {} (>= {}h old):",
            stale.len(),
            settings.repo_slug,
            settings.thresholds.max_queue_age_hours,
        )?;
        for run in stale {
            writeln!(
                stdout,
                "  - run {} ({}, branch={}) queued for {}s [{}]",
                run.run_id,
                run.workflow,
                run.branch,
                run.queued_for_secs,
                if run.cancellation_safe {
                    "cancellable"
                } else {
                    "protected: not cancellable"
                },
            )?;
        }
        if apply {
            if cancelled.is_empty() && failed.is_empty() {
                writeln!(stdout, "No eligible runs cancelled.")?;
            } else {
                writeln!(stdout, "Cancelled run ids: {cancelled:?}")?;
            }
            if !failed.is_empty() {
                writeln!(stdout, "Cancel failures:")?;
                for (id, msg) in failed {
                    writeln!(stdout, "  - run {id}: {msg}")?;
                }
            }
        } else if eligible_count > 0 {
            writeln!(stdout, "Re-run with --fix to cancel the eligible runs.")?;
        } else {
            writeln!(stdout, "No stale runs are eligible for broad cancellation.")?;
        }
        if !protected_run_ids.is_empty() {
            writeln!(
                stdout,
                "Protected run ids (not cancellable): {protected_run_ids:?}"
            )?;
        }
        Ok(())
    })();
    result.map_err(|error| CliFailure::new(1, error.to_string()))
}

fn emit_cleanup_json<W: Write>(
    stdout: &mut W,
    settings: &WatchdogSettings,
    stale: &[StaleQueuedRun],
    cancelled: &[u64],
    failed: &[(u64, String)],
    protected_run_ids: &[u64],
    apply: bool,
) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    data.insert("repo".to_owned(), Value::from(settings.repo_slug.clone()));
    data.insert(
        "stale_hours".to_owned(),
        Value::from(settings.thresholds.max_queue_age_hours),
    );
    data.insert("apply".to_owned(), Value::Bool(apply));
    data.insert(
        "stale_queued_runs".to_owned(),
        serde_json::to_value(stale).expect("stale serialization"),
    );
    data.insert(
        "cancelled_run_ids".to_owned(),
        serde_json::to_value(cancelled).expect("cancelled serialization"),
    );
    data.insert(
        "protected_run_ids".to_owned(),
        serde_json::to_value(protected_run_ids).expect("protected serialization"),
    );
    data.insert(
        "failed".to_owned(),
        Value::Array(
            failed
                .iter()
                .map(|(id, msg)| {
                    Value::Object(
                        [
                            ("run_id".to_owned(), Value::from(*id)),
                            ("error".to_owned(), Value::from(msg.clone())),
                        ]
                        .into_iter()
                        .collect(),
                    )
                })
                .collect(),
        ),
    );
    write_json_envelope(stdout, "runner.cleanup", data)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

// ---------- settings / config wiring ----------

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WatchdogSettings {
    pub(super) repo_slug: String,
    #[allow(dead_code)]
    pub(super) runner_id: Option<u64>,
    pub(super) runner_dir: PathBuf,
    pub(super) thresholds: WatchdogThresholds,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_watchdog_settings(
    config: &LoadedConfig,
    cwd: &Path,
    runner_id_override: Option<u64>,
    repo_override: Option<String>,
    runner_dir_override: Option<PathBuf>,
    max_job_min_override: Option<i64>,
    max_queue_age_hours_override: Option<i64>,
    interval_override: Option<u64>,
) -> Result<WatchdogSettings, CliFailure> {
    let repo_slug = resolve_repo_slug(repo_override, cwd)?;
    let runner_id = runner_id_override.or_else(|| {
        config
            .get("runner.watchdog.runner_id")
            .and_then(toml::Value::as_integer)
            .and_then(|value| u64::try_from(value).ok())
    });
    let runner_dir = runner_dir_override
        .or_else(|| {
            config
                .get_str("runner.watchdog.runner_dir")
                .map(PathBuf::from)
        })
        .unwrap_or_else(default_runner_dir);

    let max_job_min = max_job_min_override
        .or_else(|| {
            config
                .get("runner.watchdog.max_job_min")
                .and_then(toml::Value::as_integer)
        })
        .unwrap_or(DEFAULT_MAX_JOB_MIN);
    let max_queue_age_hours = max_queue_age_hours_override
        .or_else(|| {
            config
                .get("runner.watchdog.max_queue_age_hours")
                .and_then(toml::Value::as_integer)
        })
        .unwrap_or(DEFAULT_MAX_QUEUE_AGE_HOURS);
    let watch_interval_seconds = interval_override
        .or_else(|| {
            config
                .get("runner.watchdog.watch_interval_seconds")
                .and_then(toml::Value::as_integer)
                .and_then(|value| u64::try_from(value).ok())
        })
        .unwrap_or(DEFAULT_WATCH_INTERVAL_SECONDS);
    let auto_fix = config
        .get("runner.watchdog.auto_fix")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);

    Ok(WatchdogSettings {
        repo_slug,
        runner_id,
        runner_dir,
        thresholds: WatchdogThresholds {
            max_job_min,
            max_queue_age_hours,
            watch_interval_seconds,
            auto_fix,
        },
    })
}

fn default_runner_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("actions-runner")
    } else {
        PathBuf::from("actions-runner")
    }
}

pub(super) fn resolve_repo_slug(repo: Option<String>, cwd: &Path) -> Result<String, CliFailure> {
    if let Some(repo) = repo.filter(|value| !value.trim().is_empty()) {
        return Ok(repo);
    }
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .map_err(|error| CliFailure::new(1, format!("failed to inspect git remote: {error}")))?;
    if output.status.success() {
        let remote = String::from_utf8_lossy(&output.stdout);
        if let Some(slug) = parse_github_repo_slug(remote.trim()) {
            return Ok(slug);
        }
    }
    Err(CliFailure::new(
        1,
        "No repo detected. Pass --repo OWNER/REPO or run inside a git clone with a tracked remote.",
    ))
}

pub(super) fn parse_github_repo_slug(remote: &str) -> Option<String> {
    // Mirrors crate::app::wait_cmd::parse_github_repo_slug but kept local so
    // this module has no cross-module visibility creep.
    let trimmed = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        return slug_or_none(rest);
    }
    for prefix in [
        "https://github.com/",
        "http://github.com/",
        "ssh://git@github.com/",
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return slug_or_none(rest);
        }
    }
    None
}

fn slug_or_none(rest: &str) -> Option<String> {
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

// ---------- shell-side data collection ----------

fn fetch_runner_snapshot(
    actions: &GitHubActions,
    settings: &WatchdogSettings,
) -> Result<RunnerSnapshot, CliFailure> {
    let runner_id = settings.runner_id.ok_or_else(|| {
        CliFailure::new(
            1,
            "No runner ID configured. Pass --runner-id, or set runner.watchdog.runner_id in .shipyard/config.toml.",
        )
    })?;
    let raw = gh_api_runner(actions, &settings.repo_slug, runner_id)?;
    let parsed: Value = serde_json::from_str(&raw)
        .map_err(|error| CliFailure::new(2, format!("gh runner JSON parse failed: {error}")))?;
    let status = parsed
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let busy = parsed.get("busy").and_then(Value::as_bool).unwrap_or(false);
    let (worker_count, oldest_worker_age_min) = inspect_local_workers(&settings.runner_dir);
    Ok(RunnerSnapshot {
        status,
        busy,
        worker_count,
        oldest_worker_age_min,
    })
}

fn gh_api_runner(
    actions: &GitHubActions,
    repo: &str,
    runner_id: u64,
) -> Result<String, CliFailure> {
    let args = vec![
        "api".to_owned(),
        format!("repos/{repo}/actions/runners/{runner_id}"),
    ];
    actions
        .run_gh(&args)
        .map_err(|error| CliFailure::new(2, error.to_string()))
}

fn fetch_queued_runs(
    actions: &GitHubActions,
    repo_slug: &str,
) -> Result<Vec<QueuedRun>, CliFailure> {
    actions
        .list_queued_runs(repo_slug, QUEUED_RUNS_LIMIT)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn inspect_local_workers(runner_dir: &Path) -> (usize, Option<i64>) {
    // `ps -ax -o etime=,command=` returns lines like
    // "  12:34 /Users/foo/actions-runner/bin/Runner.Worker ...".
    let output = match Command::new("ps")
        .args(["-ax", "-o", "etime=,command="])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return (0, None),
    };
    let runner_dir_str = runner_dir.display().to_string();
    let bin_marker = format!("{runner_dir_str}/bin");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut count = 0usize;
    let mut oldest_age_min: Option<i64> = None;
    for line in stdout.lines() {
        if !line.contains("Runner.Worker") {
            continue;
        }
        if !line.contains(&bin_marker) && !line.contains(&runner_dir_str) {
            continue;
        }
        count += 1;
        let trimmed = line.trim_start();
        let first_field = trimmed.split_whitespace().next().unwrap_or("");
        if let Some(age) = parse_etime_minutes(first_field) {
            oldest_age_min = Some(oldest_age_min.map_or(age, |existing| existing.max(age)));
        }
    }
    (count, oldest_age_min)
}

/// One Runner.Worker process flagged for auto-kill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HungWorker {
    pub(super) pid: u32,
    pub(super) etime_min: i64,
}

/// Enumerate Runner.Worker processes whose etime exceeds `max_job_min`.
/// Returns oldest-first so callers can apply quotas.
pub(super) fn discover_hung_workers(runner_dir: &Path, max_job_min: i64) -> Vec<HungWorker> {
    let output = match Command::new("ps")
        .args(["-ax", "-o", "pid=,etime=,command="])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let runner_dir_str = runner_dir.display().to_string();
    let bin_marker = format!("{runner_dir_str}/bin");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut hung = Vec::new();
    for line in stdout.lines() {
        let Some(parsed) = parse_ps_pid_etime_command(line) else {
            continue;
        };
        if !parsed.command.contains("Runner.Worker") {
            continue;
        }
        if !parsed.command.contains(&bin_marker) && !parsed.command.contains(&runner_dir_str) {
            continue;
        }
        if parsed.etime_min < max_job_min {
            continue;
        }
        hung.push(HungWorker {
            pid: parsed.pid,
            etime_min: parsed.etime_min,
        });
    }
    hung.sort_by_key(|w| std::cmp::Reverse(w.etime_min));
    hung
}

#[derive(Clone, Debug)]
struct PsRow<'a> {
    pid: u32,
    etime_min: i64,
    command: &'a str,
}

/// Parse a single line of `ps -ax -o pid=,etime=,command=` output.
///
/// `ps` right-pads the PID and etime columns to fixed widths on macOS and
/// Linux, so adjacent fields are separated by *runs* of spaces — not a single
/// space. The previous `splitn(3, char::is_whitespace)` would yield an empty
/// second token whenever the gap was wider than one space, making etime
/// parsing fail and causing `discover_hung_workers` to silently miss every
/// real worker on the host (Codex P1 review against #291).
///
/// This implementation consumes whitespace runs between fields, but
/// preserves spaces inside the trailing command string.
fn parse_ps_pid_etime_command(line: &str) -> Option<PsRow<'_>> {
    let (pid_tok, after_pid) = take_token(line.trim_start())?;
    let (etime_tok, after_etime) = take_token(after_pid.trim_start())?;
    let command = after_etime.trim_start();
    if command.is_empty() {
        return None;
    }
    let pid = pid_tok.parse::<u32>().ok()?;
    let etime_min = parse_etime_minutes(etime_tok)?;
    Some(PsRow {
        pid,
        etime_min,
        command,
    })
}

/// Split off the first whitespace-delimited token. Returns `None` if `s` is
/// empty or starts with whitespace (caller must `trim_start` first).
fn take_token(s: &str) -> Option<(&str, &str)> {
    if s.is_empty() {
        return None;
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((&s[..end], &s[end..]))
}

/// Parse `ps`-style `etime` strings (`MM:SS`, `HH:MM:SS`, or `DD-HH:MM:SS`)
/// into whole minutes. Mirrors the awk pipeline in the prototype.
fn parse_etime_minutes(raw: &str) -> Option<i64> {
    let (days, hms) = if let Some((d, rest)) = raw.split_once('-') {
        (d.parse::<i64>().ok()?, rest)
    } else {
        (0, raw)
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let (hours, minutes) = match parts.as_slice() {
        [h, m, _s] => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
        [m, _s] => (0, m.parse::<i64>().ok()?),
        [m] => (0, m.parse::<i64>().ok()?),
        _ => return None,
    };
    Some(days * 24 * 60 + hours * 60 + minutes)
}

#[cfg(test)]
mod tests;
