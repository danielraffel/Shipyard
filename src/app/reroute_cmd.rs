//! CLI handler for `shipyard runner reroute-watch` (#316 Part C).
//!
//! The cloud→local macOS reroute watcher: each tick it reads free local macOS
//! capacity (#316 Part B), lists the repo's cloud-queued macOS jobs, and — when
//! a slot is free and a job is still waiting on cloud — drains one back to a
//! local runner. Decision logic + safety properties are the pure code in
//! [`crate::reroute`]; this module is the impure edge (capacity probe, `gh`
//! queries, the reroute action, the poll loop).
//!
//! **Observe by default.** Without `--apply` the watcher logs every decision but
//! takes no action — silence must not read as success, so each tick prints the
//! free-slot count, the candidates, and what it would do. `--apply` performs the
//! reroute by shelling `shipyard cloud retarget … --provider local --apply` for
//! PRs Shipyard is shipping (it reuses all the ship-state/dispatch safety).
//! Rerouting a PR with no ship-state, and spinning an **ephemeral JIT VM
//! runner** on a free-slot host, are the Part C.2 follow-up — until then a
//! persistent host-class runner handles pickup.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::thread::sleep;
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;

use super::CliFailure;
use crate::capacity::{any_unreadable, parse_host_classes, total_free};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::output::write_json_envelope;
use crate::reroute::{
    FlapGuard, RerouteCandidate, RerouteDecision, decide_reroute, macos_job_targets_cloud,
};

/// Args for `runner reroute-watch`.
pub(super) struct RerouteWatchArgs {
    /// Owner/repo slug. Defaults to the current checkout's repo.
    pub(super) repo: Option<String>,
    /// Lane/job-name substring passed to `cloud retarget --target`.
    pub(super) target: String,
    /// Seconds between polling ticks.
    pub(super) interval_secs: u64,
    /// Flap-guard window in seconds.
    pub(super) flap_window_secs: i64,
    /// Run a single tick and exit.
    pub(super) once: bool,
    /// Stop after N ticks (test hook; `None` = run forever).
    pub(super) max_ticks: Option<u32>,
    /// Actually perform reroutes (default observe-only).
    pub(super) apply: bool,
}

/// `shipyard runner reroute-watch`.
pub(super) fn reroute_watch_command<W: Write>(
    args: &RerouteWatchArgs,
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repo = match &args.repo {
        Some(r) => r.clone(),
        None => super::runner_cmd::resolve_repo_slug(None, cwd)?,
    };

    let classes = parse_host_classes(&config.data).map_err(|e| CliFailure::new(2, e))?;
    if classes.is_empty() {
        return Err(CliFailure::new(
            1,
            "No [host_class.<name>] configured — reroute-watch needs capacity hosts. \
             See `shipyard runner capacity`.",
        ));
    }

    let mut guard = FlapGuard::new(args.flap_window_secs);
    let interval = Duration::from_secs(args.interval_secs.max(1));
    let mode = if args.apply { "apply" } else { "observe" };
    if !json {
        writeln!(
            stdout,
            "reroute-watch [{mode}] repo={repo} target={} interval={}s flap_window={}s",
            args.target, args.interval_secs, args.flap_window_secs
        )
        .ok();
    }

    let mut ticks: u32 = 0;
    loop {
        // A failed tick (gh hiccup) logs and continues — never crash the loop.
        if let Err(e) = tick(args, config, actions, &repo, &mut guard, json, stdout) {
            if json {
                let mut data = BTreeMap::new();
                data.insert("error".to_owned(), Value::from(e.message.clone()));
                let _ = write_json_envelope(stdout, "runner.reroute-watch.error", data);
            } else {
                writeln!(stdout, "⚠︎ tick error (continuing): {}", e.message).ok();
            }
        }
        ticks += 1;
        if args.once || args.max_ticks.is_some_and(|max| ticks >= max) {
            break;
        }
        sleep(interval);
    }
    Ok(ExitCode::SUCCESS)
}

/// One decision tick: probe capacity, list candidates, decide, log, optionally act.
fn tick<W: Write>(
    args: &RerouteWatchArgs,
    config: &LoadedConfig,
    actions: &GitHubActions,
    repo: &str,
    guard: &mut FlapGuard,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let hosts = super::capacity_cmd::gather(config)?;
    let free = total_free(&hosts);
    let unreadable = any_unreadable(&hosts);
    let candidates = list_cloud_queued_macos(actions, repo, &args.target)?;
    let now = Utc::now();
    let decision = decide_reroute(free, &candidates, guard, now);

    // Perform the action (if applicable) before logging the outcome.
    let action = match &decision {
        RerouteDecision::Reroute(candidate) if args.apply => {
            match perform_reroute(candidate, repo, &args.target) {
                Ok(()) => {
                    guard.record(candidate.pr, now);
                    "rerouted".to_owned()
                }
                Err(reason) => format!("reroute failed: {reason}"),
            }
        }
        RerouteDecision::Reroute(_) => {
            "observe (would reroute; pass --apply to act)".to_owned()
        }
        _ => "none".to_owned(),
    };

    log_tick(
        stdout,
        json,
        repo,
        free,
        unreadable,
        &candidates,
        &decision,
        &action,
    )
}

/// Log every capacity decision + reroute — silence must not read as success.
#[allow(clippy::too_many_arguments)]
fn log_tick<W: Write>(
    stdout: &mut W,
    json: bool,
    repo: &str,
    free: u32,
    unreadable: bool,
    candidates: &[RerouteCandidate],
    decision: &RerouteDecision,
    action: &str,
) -> Result<(), CliFailure> {
    let (reason, chosen) = match decision {
        RerouteDecision::Reroute(c) => ("reroute", Some(c)),
        RerouteDecision::NoFreeSlots => ("no_free_slots", None),
        RerouteDecision::NoCandidates => ("no_candidates", None),
        RerouteDecision::AllFlapGuarded => ("all_flap_guarded", None),
    };

    if json {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::from(repo.to_owned()));
        data.insert("free_slots".to_owned(), Value::from(free));
        data.insert("any_unreadable".to_owned(), Value::from(unreadable));
        data.insert("candidates".to_owned(), Value::from(candidates.len()));
        data.insert("decision".to_owned(), Value::from(reason));
        data.insert("action".to_owned(), Value::from(action.to_owned()));
        data.insert(
            "pr".to_owned(),
            chosen.map_or(Value::Null, |c| Value::from(c.pr)),
        );
        data.insert(
            "run_id".to_owned(),
            chosen.map_or(Value::Null, |c| Value::from(c.run_id)),
        );
        return write_json_envelope(stdout, "runner.reroute-watch.tick", data)
            .map_err(|e| CliFailure::new(1, format!("failed to write JSON: {e}")));
    }

    let detail = match chosen {
        Some(c) => format!(
            "free={free} candidates={} → PR #{} (run {}) [{action}]",
            candidates.len(),
            c.pr,
            c.run_id
        ),
        None => format!(
            "free={free} candidates={} → {reason}{}",
            candidates.len(),
            if unreadable {
                " (some hosts unreadable; free is a lower bound)"
            } else {
                ""
            }
        ),
    };
    writeln!(stdout, "{detail}").ok();
    Ok(())
}

/// List cloud-queued macOS jobs as reroute candidates, sorted oldest-run-first
/// for a deterministic one-per-tick choice.
fn list_cloud_queued_macos(
    actions: &GitHubActions,
    repo: &str,
    _target: &str,
) -> Result<Vec<RerouteCandidate>, CliFailure> {
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/actions/runs?status=queued&per_page=100"),
            "--jq".to_owned(),
            "[.workflow_runs[] | {id, pr: (.pull_requests[0].number // null), \
             branch: .head_branch}] | .[]"
                .to_owned(),
        ])
        .map_err(|e| CliFailure::new(1, format!("list queued runs failed: {e}")))?;

    let mut candidates = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(run_id), Some(pr)) = (
            obj.get("id").and_then(Value::as_u64),
            obj.get("pr").and_then(Value::as_u64),
        ) else {
            continue;
        };
        let branch = obj
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if macos_job_targets_cloud(&macos_job_labels(actions, repo, run_id)) {
            candidates.push(RerouteCandidate {
                pr,
                run_id,
                head_branch: branch,
            });
        }
    }
    candidates.sort_by_key(|c| c.run_id);
    Ok(candidates)
}

/// Flattened, comma-joined label set of a run's macOS job(s). Empty string when
/// the macOS job isn't dispatched yet or the query fails (treated as
/// not-reroutable by [`macos_job_targets_cloud`]).
fn macos_job_labels(actions: &GitHubActions, repo: &str, run_id: u64) -> String {
    actions
        .run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/actions/runs/{run_id}/jobs"),
            "--jq".to_owned(),
            "[.jobs[] | select(.name | test(\"macos\"; \"i\")) | .labels] | flatten | join(\",\")"
                .to_owned(),
        ])
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Perform the reroute by shelling `shipyard cloud retarget … --provider local
/// --apply` (reuses ship-state + dispatch safety). Mirrors Pulp's watcher
/// shelling `pulp macos retarget`.
fn perform_reroute(candidate: &RerouteCandidate, repo: &str, target: &str) -> Result<(), String> {
    let exe = std::env::current_exe().unwrap_or_else(|_| "shipyard".into());
    let output = Command::new(exe)
        .args([
            "cloud",
            "retarget",
            "--pr",
            &candidate.pr.to_string(),
            "--target",
            target,
            "--provider",
            "local",
            "--repo",
            repo,
            "--apply",
        ])
        .output()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(stderr
        .lines()
        .next()
        .unwrap_or("cloud retarget failed")
        .to_owned())
}
