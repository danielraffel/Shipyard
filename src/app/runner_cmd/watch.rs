use super::{
    BTreeMap, CliFailure, DEFAULT_REAP_IN_PROGRESS_MAX_MIN, DEFAULT_REAP_QUEUED_MAX_MIN, Duration,
    ExitCode, GitHubActions, LoadedConfig, Path, PathBuf, REAP_RUNS_MAX_PAGES, ReaperThresholds,
    RunnerCommand, RunnerHealth, RunnerReport, StaleRun, Utc, Value, WatchdogSettings, Write,
    assess_runner, compute_stale_runs, discover_hung_workers, fetch_queued_runs,
    fetch_runner_snapshot, fleet_liveness_policy, report_to_json, resolve_watchdog_settings, sleep,
    write_json_envelope,
};

// ---------- watch ----------

#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_watch<W: Write>(
    command: RunnerCommand,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let RunnerCommand::Watch {
        runner_id,
        repo,
        runner_dir,
        interval,
        fleet_base,
        fix,
        kill_hung_workers,
        reap_stale_runs,
        reap_in_progress_max_min,
        reap_queued_max_min,
        dry_run,
        max_iterations,
        kill_grace_secs,
    } = command
    else {
        unreachable!("dispatch_watch only handles Watch")
    };
    watch_command(WatchCommandArgs {
        config,
        cwd,
        state_dir,
        actions,
        runner_id_override: runner_id,
        repo_override: repo,
        runner_dir_override: runner_dir,
        interval_override: interval,
        fleet_base_override: fleet_base,
        fix: fix || kill_hung_workers,
        kill_hung_workers,
        reap_stale_runs,
        reap_in_progress_max_min,
        reap_queued_max_min,
        dry_run,
        max_iterations,
        kill_grace_secs,
        json,
        stdout,
    })
}

#[allow(clippy::struct_excessive_bools)]
pub(super) struct WatchCommandArgs<'a, W: Write> {
    pub(super) config: &'a LoadedConfig,
    pub(super) cwd: &'a Path,
    pub(super) state_dir: &'a Path,
    pub(super) actions: &'a GitHubActions,
    pub(super) runner_id_override: Option<u64>,
    pub(super) repo_override: Option<String>,
    pub(super) runner_dir_override: Option<PathBuf>,
    pub(super) interval_override: Option<u64>,
    pub(super) fleet_base_override: Option<String>,
    pub(super) fix: bool,
    pub(super) kill_hung_workers: bool,
    pub(super) reap_stale_runs: bool,
    pub(super) reap_in_progress_max_min: Option<i64>,
    pub(super) reap_queued_max_min: Option<i64>,
    pub(super) dry_run: bool,
    pub(super) max_iterations: Option<u32>,
    pub(super) kill_grace_secs: Option<u64>,
    pub(super) json: bool,
    pub(super) stdout: &'a mut W,
}

#[allow(clippy::too_many_lines)]
pub(super) fn watch_command<W: Write>(
    args: WatchCommandArgs<'_, W>,
) -> Result<ExitCode, CliFailure> {
    let WatchCommandArgs {
        config,
        cwd,
        state_dir,
        actions,
        runner_id_override,
        repo_override,
        runner_dir_override,
        interval_override,
        fleet_base_override,
        fix,
        kill_hung_workers,
        reap_stale_runs,
        reap_in_progress_max_min,
        reap_queued_max_min,
        dry_run,
        max_iterations,
        kill_grace_secs,
        json,
        stdout,
    } = args;
    if max_iterations == Some(0) {
        return Ok(ExitCode::SUCCESS);
    }
    let settings = resolve_watchdog_settings(
        config,
        cwd,
        runner_id_override,
        repo_override.clone(),
        runner_dir_override.clone(),
        None,
        None,
        interval_override,
    )?;
    let reaper_thresholds =
        resolve_reaper_thresholds(config, reap_in_progress_max_min, reap_queued_max_min);
    let interval = Duration::from_secs(settings.thresholds.watch_interval_seconds.max(1));
    let mut fleet_base = None;
    let mut iterations = 0u32;
    let mut fleet_failed = false;
    let last_health = loop {
        let snapshot_result = fetch_runner_snapshot(actions, &settings);
        let queued_runs_result = fetch_queued_runs(actions, &settings.repo_slug);

        let health = match (snapshot_result, queued_runs_result) {
            (Ok(snapshot), Ok(queued_runs)) => {
                let report =
                    assess_runner(&snapshot, &queued_runs, settings.thresholds, Utc::now());
                emit_watch_tick(stdout, &settings, &report, json)?;
                if (fix || settings.thresholds.auto_fix) && report.health == RunnerHealth::Stuck {
                    cancel_stale_inline(actions, &settings, &report, stdout, json)?;
                }
                if kill_hung_workers && report_has_hung_worker(&report) {
                    auto_kill_hung_workers(
                        config,
                        cwd,
                        actions,
                        &settings,
                        kill_grace_secs,
                        json,
                        stdout,
                    )?;
                }
                report.health
            }
            (Err(err), _) | (_, Err(err)) => {
                emit_watch_error(stdout, &settings, &err, json)?;
                RunnerHealth::Offline
            }
        };
        if reap_stale_runs
            && let Err(error) =
                reap_stale_runs_tick(actions, &settings, reaper_thresholds, dry_run, json, stdout)
        {
            fleet_failed = true;
            emit_watch_error(stdout, &settings, &error, json)?;
        }

        if fleet_liveness_due(config, iterations) {
            let base = fleet_base.clone().map_or_else(
                || {
                    resolve_fleet_base(
                        actions,
                        config,
                        &settings.repo_slug,
                        fleet_base_override.clone(),
                    )
                },
                Ok,
            );
            let result = base.and_then(|base| {
                fleet_base = Some(base.clone());
                let fleet_args = super::super::fleet_status_cmd::FleetStatusArgs {
                    repo: Some(settings.repo_slug.clone()),
                    base,
                    target: "macos".to_owned(),
                    queued_age_threshold_secs: 900,
                    queue_run_limit: 100,
                    merge_queue_stall_threshold_secs: 900,
                    release_stale_threshold_secs: 86_400,
                };
                let assessment = super::super::fleet_status_cmd::collect_fleet_assessment(
                    fleet_args, config, cwd, state_dir, actions,
                )?;
                if json {
                    super::super::fleet_status_cmd::render_fleet_watch_event(&assessment, stdout)?;
                } else {
                    super::super::fleet_status_cmd::render_fleet_assessment(
                        &assessment,
                        false,
                        stdout,
                    )?;
                }
                Ok(assessment.exit_code())
            });
            match result {
                Ok(code) => fleet_failed |= code != ExitCode::SUCCESS,
                Err(error) => {
                    fleet_failed = true;
                    emit_watch_error(stdout, &settings, &error, json)?;
                }
            }
        }

        iterations = iterations.saturating_add(1);
        if let Some(limit) = max_iterations
            && iterations >= limit
        {
            break health;
        }
        sleep(interval);
    };
    Ok(ExitCode::from(watch_exit_code(last_health, fleet_failed)))
}

pub(super) fn watch_exit_code(health: RunnerHealth, fleet_failed: bool) -> u8 {
    if fleet_failed && health == RunnerHealth::Healthy {
        1
    } else {
        health.exit_code()
    }
}

pub(super) fn fleet_liveness_due(config: &LoadedConfig, iteration: u32) -> bool {
    fleet_liveness_policy(config).is_due(iteration)
}

pub(super) fn resolve_fleet_base(
    actions: &GitHubActions,
    config: &LoadedConfig,
    repo: &str,
    override_base: Option<String>,
) -> Result<String, CliFailure> {
    if let Some(base) = override_base.or_else(|| {
        config
            .get("runner.watchdog.fleet_base")
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    }) {
        return Ok(base);
    }
    let raw = actions
        .run_gh(&["api".to_owned(), format!("repos/{repo}")])
        .map_err(|error| CliFailure::new(1, format!("resolve fleet base failed: {error}")))?;
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|value| {
            value
                .get("default_branch")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| CliFailure::new(1, "repository response missing default_branch"))
}

pub(super) fn report_has_hung_worker(report: &crate::runner_watchdog::RunnerReport) -> bool {
    use crate::runner_watchdog::Symptom;
    report
        .symptoms
        .iter()
        .any(|s| matches!(s, Symptom::HungWorker { .. }))
}

pub(super) fn auto_kill_hung_workers<W: Write>(
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    settings: &WatchdogSettings,
    grace_secs: Option<u64>,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let workers = discover_hung_workers(&settings.runner_dir, settings.thresholds.max_job_min);
    if workers.is_empty() {
        emit_kill_event(
            stdout,
            &settings.repo_slug,
            json,
            "no-pid-found",
            None,
            None,
        )?;
        return Ok(());
    }
    for worker in workers {
        let reason = format!(
            "watchdog: worker etime {}min exceeds threshold {}min",
            worker.etime_min, settings.thresholds.max_job_min
        );
        emit_kill_event(
            stdout,
            &settings.repo_slug,
            json,
            "attempt",
            Some(worker.pid),
            Some(&reason),
        )?;
        let kill_args = super::super::runner_kill_cmd::KillCommandArgs {
            config,
            cwd,
            actions,
            pid: Some(worker.pid),
            reason: Some(reason.clone()),
            retrigger: false,
            yes: true,
            repo_override: Some(settings.repo_slug.clone()),
            runner_dir_override: Some(settings.runner_dir.clone()),
            history: false,
            last: None,
            recover: None,
            grace_secs,
            recovery_log_override: None,
            quarantine_root_override: None,
            no_wait_github: false,
            json,
        };
        let outcome = super::super::runner_kill_cmd::kill_command(kill_args, stdout);
        match outcome {
            Ok(_code) => emit_kill_event(
                stdout,
                &settings.repo_slug,
                json,
                "killed",
                Some(worker.pid),
                None,
            )?,
            Err(err) => emit_kill_event(
                stdout,
                &settings.repo_slug,
                json,
                "failed",
                Some(worker.pid),
                Some(&err.message),
            )?,
        }
    }
    Ok(())
}

pub(super) fn emit_kill_event<W: Write>(
    stdout: &mut W,
    repo: &str,
    json: bool,
    phase: &str,
    pid: Option<u32>,
    detail: Option<&str>,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("auto_kill_worker"));
        data.insert("phase".to_owned(), Value::from(phase.to_owned()));
        data.insert("repo".to_owned(), Value::from(repo.to_owned()));
        if let Some(pid) = pid {
            data.insert("pid".to_owned(), Value::from(pid));
        }
        if let Some(detail) = detail {
            data.insert("detail".to_owned(), Value::from(detail.to_owned()));
        }
        return write_json_envelope(stdout, "runner.watch", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    let ts = Utc::now().format("%H:%M:%S");
    let pid_part = pid.map_or_else(String::new, |p| format!(" pid={p}"));
    let detail_part = detail.map_or_else(String::new, |d| format!(" — {d}"));
    writeln!(stdout, "[{ts}] auto-kill {phase}{pid_part}{detail_part}")
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

pub(super) fn emit_watch_tick<W: Write>(
    stdout: &mut W,
    settings: &WatchdogSettings,
    report: &RunnerReport,
    json: bool,
) -> Result<(), CliFailure> {
    if json {
        let mut data = report_to_json(report);
        data.insert("event".to_owned(), Value::from("tick"));
        data.insert("repo".to_owned(), Value::from(settings.repo_slug.clone()));
        return write_json_envelope(stdout, "runner.watch", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    let ts = Utc::now().format("%H:%M:%S");
    let line = match report.health {
        RunnerHealth::Healthy => format!(
            "[{ts}] OK: runner healthy (busy={}, workers={}, stale=0)",
            report.busy, report.worker_count,
        ),
        RunnerHealth::Stuck => format!(
            "[{ts}] WARN: stuck runner — {} symptom(s); {} stale queued",
            report.symptoms.len(),
            report.stale_queued_runs.len(),
        ),
        RunnerHealth::Offline => format!("[{ts}] ERR: runner status={}", report.status),
    };
    writeln!(stdout, "{line}").map_err(|error| CliFailure::new(1, error.to_string()))
}

pub(super) fn emit_watch_error<W: Write>(
    stdout: &mut W,
    settings: &WatchdogSettings,
    err: &CliFailure,
    json: bool,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("error"));
        data.insert("repo".to_owned(), Value::from(settings.repo_slug.clone()));
        data.insert("error".to_owned(), Value::from(err.message.clone()));
        return write_json_envelope(stdout, "runner.watch", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    let ts = Utc::now().format("%H:%M:%S");
    writeln!(stdout, "[{ts}] ERR: {}", err.message)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

pub(super) fn cancel_stale_inline<W: Write>(
    actions: &GitHubActions,
    settings: &WatchdogSettings,
    report: &RunnerReport,
    stdout: &mut W,
    json: bool,
) -> Result<(), CliFailure> {
    let mut cancelled = Vec::new();
    let mut failed = Vec::new();
    for run in &report.stale_queued_runs {
        match actions.cancel_workflow_run(&settings.repo_slug, run.run_id) {
            Ok(()) => cancelled.push(run.run_id),
            Err(err) => failed.push((run.run_id, err.to_string())),
        }
    }
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("auto_fix"));
        data.insert("repo".to_owned(), Value::from(settings.repo_slug.clone()));
        data.insert(
            "cancelled_run_ids".to_owned(),
            serde_json::to_value(&cancelled).expect("cancelled serialization"),
        );
        data.insert(
            "failed".to_owned(),
            serde_json::to_value(
                failed
                    .iter()
                    .map(|(id, msg)| {
                        BTreeMap::from([
                            ("run_id".to_owned(), Value::from(*id)),
                            ("error".to_owned(), Value::from(msg.clone())),
                        ])
                    })
                    .collect::<Vec<_>>(),
            )
            .expect("failed serialization"),
        );
        return write_json_envelope(stdout, "runner.watch", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    if !cancelled.is_empty() {
        writeln!(stdout, "  auto-fix: cancelled {cancelled:?}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if !failed.is_empty() {
        for (id, msg) in failed {
            writeln!(stdout, "  auto-fix FAILED for run {id}: {msg}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

// ---------- stale-run reaper ----------

/// Resolve [`ReaperThresholds`] from flags, then `[runner.watchdog]` config,
/// then the built-in defaults — matching the precedence used by
/// `resolve_watchdog_settings`.
pub(super) fn resolve_reaper_thresholds(
    config: &LoadedConfig,
    in_progress_override: Option<i64>,
    queued_override: Option<i64>,
) -> ReaperThresholds {
    let in_progress_max_min = in_progress_override
        .or_else(|| {
            config
                .get("runner.watchdog.reap_in_progress_max_min")
                .and_then(toml::Value::as_integer)
        })
        .unwrap_or(DEFAULT_REAP_IN_PROGRESS_MAX_MIN);
    let queued_max_min = queued_override
        .or_else(|| {
            config
                .get("runner.watchdog.reap_queued_max_min")
                .and_then(toml::Value::as_integer)
        })
        .unwrap_or(DEFAULT_REAP_QUEUED_MAX_MIN);
    ReaperThresholds {
        in_progress_max_min,
        queued_max_min,
    }
}

/// One stale-run reaper pass: list `in_progress` + `queued` runs, select the
/// genuinely-stale ones, and cancel them (unless `--dry-run`). Emits one
/// `event=reap_stale_run` envelope per run, mirroring the `auto_kill_worker`
/// event style used by `--kill-hung-workers`.
pub(super) fn reap_stale_runs_tick<W: Write>(
    actions: &GitHubActions,
    settings: &WatchdogSettings,
    thresholds: ReaperThresholds,
    dry_run: bool,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    // Paginate both status queries so repos with more than one page of
    // `in_progress` / `queued` runs are fully covered — a single 100-item
    // page would silently miss the oldest (and most likely stale) entries.
    let in_progress = actions
        .list_runs_with_status_paginated(
            &settings.repo_slug,
            "in_progress",
            None,
            REAP_RUNS_MAX_PAGES,
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let queued = actions
        .list_runs_with_status_paginated(&settings.repo_slug, "queued", None, REAP_RUNS_MAX_PAGES)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;

    let stale = compute_stale_runs(&in_progress, &queued, thresholds, Utc::now());
    let mut failures = Vec::new();
    for run in &stale {
        let detail = format!(
            "{} run {} ({}, branch={}) {} for {}s — threshold {}min",
            run.kind.as_str(),
            run.run_id,
            run.workflow,
            run.branch,
            run.status,
            run.age_secs,
            match run.kind {
                crate::runner_watchdog::StaleRunKind::HungInProgress => {
                    thresholds.in_progress_max_min
                }
                crate::runner_watchdog::StaleRunKind::OrphanedQueued => thresholds.queued_max_min,
            },
        );
        emit_reap_event(
            stdout,
            &settings.repo_slug,
            json,
            "attempt",
            run,
            Some(&detail),
        )?;
        if dry_run {
            emit_reap_event(
                stdout,
                &settings.repo_slug,
                json,
                "skipped",
                run,
                Some("dry-run: not cancelling"),
            )?;
            continue;
        }
        match actions.cancel_workflow_run(&settings.repo_slug, run.run_id) {
            Ok(()) => {
                emit_reap_event(stdout, &settings.repo_slug, json, "cancelled", run, None)?;
            }
            Err(err) => {
                emit_reap_event(
                    stdout,
                    &settings.repo_slug,
                    json,
                    "failed",
                    run,
                    Some(&err.to_string()),
                )?;
                failures.push(format!("run {}: {err}", run.run_id));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!(
                "{} stale-run cancellation(s) failed: {}",
                failures.len(),
                failures.join("; ")
            ),
        ))
    }
}

pub(super) fn emit_reap_event<W: Write>(
    stdout: &mut W,
    repo: &str,
    json: bool,
    phase: &str,
    run: &StaleRun,
    detail: Option<&str>,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("reap_stale_run"));
        data.insert("phase".to_owned(), Value::from(phase.to_owned()));
        data.insert("repo".to_owned(), Value::from(repo.to_owned()));
        data.insert("run_id".to_owned(), Value::from(run.run_id));
        data.insert("kind".to_owned(), Value::from(run.kind.as_str()));
        data.insert("status".to_owned(), Value::from(run.status.clone()));
        data.insert("workflow".to_owned(), Value::from(run.workflow.clone()));
        data.insert("branch".to_owned(), Value::from(run.branch.clone()));
        data.insert("age_secs".to_owned(), Value::from(run.age_secs));
        if let Some(url) = &run.url {
            data.insert("url".to_owned(), Value::from(url.clone()));
        }
        if let Some(detail) = detail {
            data.insert("detail".to_owned(), Value::from(detail.to_owned()));
        }
        return write_json_envelope(stdout, "runner.watch", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    let ts = Utc::now().format("%H:%M:%S");
    let detail_part = detail.map_or_else(String::new, |d| format!(" — {d}"));
    writeln!(
        stdout,
        "[{ts}] reap-stale-run {phase} run={}{detail_part}",
        run.run_id
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}
