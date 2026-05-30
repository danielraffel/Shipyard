use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

use super::{
    CliFailure,
    auto_merge_cmd::{AutoMergeOutcome, AutoMergeRequest, execute_auto_merge},
    cli::{MergeMethod, MergeResult},
    wait_cmd::parse_github_repo_slug,
};
use crate::config::LoadedConfig;
use crate::diagnostics::{
    FailureDiagnostics, FailureKind, GhDiagnosticsFetcher, fetch_failed_job_diagnostics,
    select_parser,
};
use crate::evidence::EvidenceStore;
use crate::executor::dispatch::{ExecutorDispatcher, ResolvedTarget, resolve_targets};
use crate::governance::{GovernanceGh, put_branch_protection, resolve_branch_rules};
use crate::job::{Job, Priority, TargetResult, TargetStatus, ValidationMode};
use crate::lane_policy::{LanePolicy, resolve_lane_policy};
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::pr::{PrInfo, create_pr, find_pr_for_branch, push_branch};
use crate::pr_text::{compose_pr_body_with_policy, compose_pr_title};
use crate::preflight::{
    EXIT_BACKEND_UNREACHABLE, ShipPreflightError, ShipPreflightOptions,
    collect_ship_preflight_with_options,
};
use crate::prepared_state::PreparedStateStore;
use crate::queue::Queue;
use crate::ship::{ShipExecutionRequest, ShipStores, drain_or_wait_ship, submit_ship};
use crate::ship_state::ShipStateStore;
use crate::warm_pool::{WarmPool, default_pool_path};

pub(super) struct ShipCommandArgs {
    pub(super) pr: Option<u64>,
    pub(super) base: String,
    pub(super) auto_create_base: Option<bool>,
    pub(super) no_warm: bool,
    pub(super) resume_from: Option<String>,
    pub(super) merge_command: Option<PathBuf>,
    pub(super) merge_result: Option<MergeResult>,
    pub(super) gh_command: Option<PathBuf>,
    /// Test hook: bypass `gh pr view` for archived-PR checks in the
    /// auto-merge handoff. Mirrors `auto-merge --pr-snapshot-file`. See
    /// Shipyard issue #296 for the failure mode this guards against.
    pub(super) pr_snapshot_file: Option<PathBuf>,
    pub(super) allow_unreachable_targets: bool,
    pub(super) skip_targets: Vec<String>,
}

#[allow(clippy::too_many_lines)]
pub(super) fn ship_command<W: Write>(
    args: ShipCommandArgs,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let preflight_dispatcher = ExecutorDispatcher::new(None);
    let targets = prepare_ship_targets(
        config,
        cwd,
        runtime_paths,
        &preflight_dispatcher,
        &args,
        json_mode,
        stdout,
    )?;

    let branch = git_required(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let sha = git_required(cwd, &["rev-parse", "HEAD"])?;
    let commit_subject =
        git_optional(cwd, &["log", "-1", "--format=%s", "HEAD"]).unwrap_or_default();
    let repo = git_repo_slug(cwd).unwrap_or_default();
    if should_auto_create_base(&args.base, args.auto_create_base) {
        maybe_auto_create_base_branch(cwd, &args.base, config, args.gh_command.as_deref());
    }
    let lane_policy = resolve_lane_policy(config, cwd);
    let pr_context = resolve_pr_context(
        args.pr,
        &args.base,
        cwd,
        &branch,
        args.gh_command.as_deref(),
        &lane_policy,
    )?;

    let mut queue = Queue::new(runtime_paths.state_dir.clone())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let evidence = EvidenceStore::new(runtime_paths.state_dir.join("evidence"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let ship_state = ShipStateStore::new(runtime_paths.state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let prepared = PreparedStateStore::new(runtime_paths.state_dir.join("prepared"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let warm_pool = WarmPool::new(default_pool_path(&runtime_paths.state_dir));
    let dispatcher =
        ExecutorDispatcher::new_with_state_dir(Some(prepared), &runtime_paths.state_dir);
    let request = ShipExecutionRequest {
        pr: pr_context.number,
        repo,
        branch,
        base_branch: pr_context.base_branch,
        sha,
        commit_subject,
        pr_url: pr_context.pr_url,
        pr_title: pr_context.pr_title,
        mode: ValidationMode::Full,
        priority: Priority::Normal,
        warm_disabled: args.no_warm,
        fail_fast: false,
        resume_from: args.resume_from,
        advisory_targets: lane_policy.advisory_targets.clone(),
        targets,
    };

    let job = submit_ship(&request, &mut queue, cwd, &runtime_paths.state_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let outcome = drain_or_wait_ship(
        &request,
        job.clone(),
        ShipStores {
            queue: &mut queue,
            evidence: &evidence,
            ship_state: &ship_state,
            warm_pool: &warm_pool,
            cwd,
            state_dir: &runtime_paths.state_dir,
        },
        &dispatcher,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;

    let render_state = post_run_merge_state(
        pr_context.number,
        cwd,
        &ship_state,
        outcome.job.passed(),
        args.merge_command,
        args.merge_result,
        args.pr_snapshot_file,
    )?;
    // Issue #303: when validation failed, resolve failing-job + log diagnostics
    // before we render so the human / JSON output points the user at the
    // failing test list, not just "Validation failed".
    let diagnostics = if render_state == ShipRenderState::ValidationFailed {
        collect_failure_diagnostics(&request.repo, &outcome.job, cwd, config)
    } else {
        Vec::new()
    };
    if json_mode {
        render_json(
            stdout,
            pr_context.number,
            &outcome,
            render_state.merged(),
            &diagnostics,
        )?;
    } else {
        render_human(stdout, pr_context.number, &render_state, &diagnostics)?;
    }
    Ok(if render_state == ShipRenderState::ValidationFailed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// One element per failed target. Built by `collect_failure_diagnostics`.
#[derive(Clone, Debug)]
pub(super) struct RenderedDiagnostics {
    pub(super) target: TargetResult,
    pub(super) kind: FailureKind,
    pub(super) details: Option<FailureDiagnostics>,
}

fn collect_failure_diagnostics(
    repo: &str,
    job: &Job,
    cwd: &Path,
    config: &LoadedConfig,
) -> Vec<RenderedDiagnostics> {
    let fetcher = GhDiagnosticsFetcher::from_loaded_config(cwd, config);
    let mut out = Vec::new();
    for result in job.results.values() {
        if matches!(
            result.status,
            TargetStatus::Pass | TargetStatus::Pending | TargetStatus::Running
        ) {
            continue;
        }
        let kind = match result.status {
            TargetStatus::Cancelled => FailureKind::Cancelled,
            // FailureClass::Timeout maps to TargetStatus::Error today; the
            // executor sets the human error_message accordingly. We classify
            // by the failure_class string when present.
            TargetStatus::Error if result.failure_class.as_deref() == Some("timeout") => {
                FailureKind::TimedOut
            }
            _ => FailureKind::Failed,
        };
        let mut target = result.clone();
        let details = if let (Some(run_id), Some(slug)) =
            (result.cloud_run_id, (!repo.is_empty()).then_some(repo))
        {
            let parser = select_parser(result.failure_parser.as_deref());
            let resolved = fetch_failed_job_diagnostics(
                &fetcher,
                slug,
                run_id,
                &result.target_name,
                parser.as_ref(),
            );
            if let Some(job) = resolved.job.as_ref() {
                target.cloud_job_id = Some(job.job_id);
                target.cloud_job_name = Some(job.name.clone());
                target.cloud_job_url = Some(job.html_url.clone());
                target.cloud_failed_step.clone_from(&job.failed_step);
            }
            Some(resolved)
        } else {
            None
        };
        out.push(RenderedDiagnostics {
            target,
            kind,
            details,
        });
    }
    out
}

fn prepare_ship_targets<W: Write>(
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    preflight_dispatcher: &ExecutorDispatcher,
    args: &ShipCommandArgs,
    json_mode: bool,
    stdout: &mut W,
) -> Result<Vec<ResolvedTarget>, CliFailure> {
    let resolved = resolve_targets(config, ValidationMode::Full)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let skipped_targets = skipped_present(&resolved, &args.skip_targets)?;
    let targets = select_targets(resolved, &args.skip_targets);
    if targets.is_empty() {
        return Err(CliFailure::new(
            2,
            "No targets remain after --skip-target filtering.",
        ));
    }
    let mut preflight = collect_ship_preflight_with_options(
        config,
        cwd,
        &runtime_paths.state_dir,
        &targets,
        preflight_dispatcher,
        ShipPreflightOptions {
            allow_root_mismatch: false,
            allow_unreachable_targets: args.allow_unreachable_targets,
        },
    )
    .map_err(|error| preflight_failure(&error))?;
    for skipped in &skipped_targets {
        preflight.warnings.push(format!(
            "Target '{skipped}' deliberately skipped (--skip-target)."
        ));
    }
    preflight.skipped_targets = skipped_targets;
    if !json_mode {
        for warning in &preflight.warnings {
            writeln!(stdout, "warning: {warning}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(targets)
}

fn preflight_failure(error: &ShipPreflightError) -> CliFailure {
    let code = match error {
        ShipPreflightError::RootMismatch { .. } => 1,
        ShipPreflightError::BackendUnreachable { .. } => EXIT_BACKEND_UNREACHABLE,
    };
    CliFailure::new(code, error.to_string())
}

fn select_targets(resolved: Vec<ResolvedTarget>, skip_targets: &[String]) -> Vec<ResolvedTarget> {
    let skip = skip_targets
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    resolved
        .into_iter()
        .filter(|target| !skip.contains(target.name.as_str()))
        .collect()
}

fn skipped_present(
    resolved: &[ResolvedTarget],
    skip_targets: &[String],
) -> Result<Vec<String>, CliFailure> {
    let known_targets = resolved
        .iter()
        .map(|target| target.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut skipped = Vec::new();
    let mut missing = Vec::new();
    for name in skip_targets {
        if known_targets.contains(name.as_str()) {
            skipped.push(name.clone());
        } else {
            missing.push(name.clone());
        }
    }
    if !missing.is_empty() {
        missing.sort();
        return Err(CliFailure::new(
            1,
            format!(
                "skip-target names no configured target: {}",
                missing.join(", ")
            ),
        ));
    }
    Ok(skipped)
}

fn should_auto_create_base(base: &str, flag: Option<bool>) -> bool {
    flag.unwrap_or_else(|| base.starts_with("develop/") || base.starts_with("release/"))
}

fn maybe_auto_create_base_branch(
    cwd: &Path,
    base: &str,
    config: &LoadedConfig,
    gh_command: Option<&Path>,
) {
    match origin_branch_exists(cwd, base) {
        Some(false) => {}
        Some(true) | None => return,
    }
    let Some(base_sha) = origin_branch_sha(cwd, "main") else {
        return;
    };
    let refspec = format!("{base_sha}:refs/heads/{base}");
    let Ok(push) = crate::supervised::git_supervised()
        .args(["push", "origin", &refspec])
        .current_dir(cwd)
        .output()
    else {
        return;
    };
    if !push.status.success() {
        return;
    }
    let Some(repo) = git_repo_slug(cwd) else {
        return;
    };
    let Ok(rules) = resolve_branch_rules(&config.data, base) else {
        return;
    };
    let Ok(gh) = GovernanceGh::from_loaded_config(cwd, config, gh_command) else {
        return;
    };
    let _ = put_branch_protection(&repo, base, &rules, &gh);
}

fn origin_branch_exists(cwd: &Path, branch: &str) -> Option<bool> {
    let output = crate::supervised::git_supervised()
        .args(["ls-remote", "--exit-code", "--heads", "origin", branch])
        .current_dir(cwd)
        .output()
        .ok()?;
    Some(output.status.success())
}

fn origin_branch_sha(cwd: &Path, branch: &str) -> Option<String> {
    let output = crate::supervised::git_supervised()
        .args([
            "ls-remote",
            "--exit-code",
            "origin",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.split_whitespace().next().map(str::to_owned)
}

struct ResolvedPrContext {
    number: u64,
    base_branch: String,
    pr_url: Option<String>,
    pr_title: Option<String>,
}

fn resolve_pr_context(
    pr: Option<u64>,
    base: &str,
    cwd: &Path,
    branch: &str,
    gh_command: Option<&Path>,
    lane_policy: &LanePolicy,
) -> Result<ResolvedPrContext, CliFailure> {
    if let Some(number) = pr {
        return Ok(ResolvedPrContext {
            number,
            base_branch: base.to_owned(),
            pr_url: None,
            pr_title: None,
        });
    }

    push_branch(cwd, branch).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let info = find_pr_for_branch(cwd, gh_command, branch)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .map_or_else(
            || create_current_branch_pr(cwd, gh_command, branch, base, lane_policy),
            Ok::<PrInfo, CliFailure>,
        )?;
    Ok(ResolvedPrContext {
        number: info.number,
        base_branch: info.base,
        pr_url: Some(info.url),
        pr_title: Some(info.title),
    })
}

fn create_current_branch_pr(
    cwd: &Path,
    gh_command: Option<&Path>,
    branch: &str,
    base: &str,
    lane_policy: &LanePolicy,
) -> Result<PrInfo, CliFailure> {
    create_pr(
        cwd,
        gh_command,
        branch,
        base,
        &compose_pr_title(cwd, branch),
        &compose_pr_body_with_policy(cwd, Some(lane_policy)),
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ShipRenderState {
    ValidationFailed,
    Merged,
    /// Shipyard's locally-supervised targets all passed, but the
    /// downstream `gh pr merge` call was rejected — typically because
    /// GitHub branch protection requires checks that are still in
    /// flight (issue #301 2/3). The wrapped string is the error
    /// from the merge attempt, useful for human + JSON renderers
    /// to surface the actual reason instead of claiming "all green".
    GreenNotMerged(String),
}

impl ShipRenderState {
    fn merged(&self) -> bool {
        matches!(self, Self::Merged)
    }
}

fn post_run_merge_state(
    pr: u64,
    cwd: &Path,
    store: &ShipStateStore,
    validation_passed: bool,
    merge_command: Option<PathBuf>,
    merge_result: Option<MergeResult>,
    pr_snapshot_file: Option<PathBuf>,
) -> Result<ShipRenderState, CliFailure> {
    if !validation_passed {
        return Ok(ShipRenderState::ValidationFailed);
    }
    let request = AutoMergeRequest {
        pr,
        merge_method: MergeMethod::Squash,
        delete_branch: true,
        admin: false,
        pr_snapshot_file,
        merge_command,
        merge_result,
    };
    match execute_auto_merge(store, cwd, &request)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
    {
        AutoMergeOutcome::Merged { .. } | AutoMergeOutcome::AlreadyMerged => {
            Ok(ShipRenderState::Merged)
        }
        AutoMergeOutcome::MergeFailed { error } => Ok(ShipRenderState::GreenNotMerged(error)),
        // Validation passed but the live head advanced past the validated SHA
        // (issue #321). Report green-not-merged so `shipyard ship` surfaces
        // merged:false / status:"green_not_merged" and the operator re-ships
        // to validate the new head.
        AutoMergeOutcome::SupersededSha { validated, current } => {
            Ok(ShipRenderState::GreenNotMerged(format!(
                "live PR head {current} superseded the validated SHA {validated}; re-run shipyard ship to validate the new head"
            )))
        }
        AutoMergeOutcome::PrNotFound
        | AutoMergeOutcome::InFlight { .. }
        | AutoMergeOutcome::TargetFailed { .. } => Err(CliFailure::new(
            1,
            format!("PR #{pr}: validation passed but ship-state was not merge-ready"),
        )),
    }
}

fn render_json<W: Write>(
    stdout: &mut W,
    pr: u64,
    outcome: &crate::ship::ShipExecutionOutcome,
    merged: bool,
    diagnostics: &[RenderedDiagnostics],
) -> Result<(), CliFailure> {
    let diag_payload: Vec<Value> = diagnostics
        .iter()
        .map(|entry| {
            json!({
                "failed_target": entry.target.target_name,
                "status": entry.target.status,
                "kind": failure_kind_label(entry.kind),
                "cloud_run_id": entry.target.cloud_run_id,
                "cloud_job_id": entry.target.cloud_job_id,
                "cloud_job_url": entry.target.cloud_job_url,
                "failed_step": entry.target.cloud_failed_step,
                "details": entry.details,
            })
        })
        .collect();
    write_json_envelope(
        stdout,
        "ship",
        fields([
            ("pr", Value::from(pr)),
            ("merged", Value::Bool(merged)),
            ("run", outcome.job.to_json_value()),
            ("ship_state", json!(outcome.ship_state)),
            (
                "resumed_existing_state",
                Value::Bool(outcome.resumed_existing_state),
            ),
            ("diagnostics", Value::Array(diag_payload)),
        ]),
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn render_human<W: Write>(
    stdout: &mut W,
    pr: u64,
    state: &ShipRenderState,
    diagnostics: &[RenderedDiagnostics],
) -> Result<(), CliFailure> {
    let result = match state {
        ShipRenderState::ValidationFailed => render_validation_failed(stdout, pr, diagnostics),
        ShipRenderState::Merged => writeln!(stdout, "PR #{pr} merged. All green."),
        ShipRenderState::GreenNotMerged(error) => render_green_not_merged(stdout, pr, error),
    };
    result.map_err(|error| CliFailure::new(1, error.to_string()))
}

/// Issue #301 (2/3). The previous render claimed "All green but
/// merge failed" — misleading when the actual cause is GitHub
/// branch protection waiting on checks Shipyard doesn't supervise
/// (e.g. GHA-hosted Linux/Windows still `in_progress` while local
/// macOS already passed). Surface the underlying error verbatim
/// and point the user at the two unblocks they can pick from.
fn render_green_not_merged<W: Write>(stdout: &mut W, pr: u64, error: &str) -> std::io::Result<()> {
    writeln!(
        stdout,
        "Shipyard-validated targets passed, but the merge attempt was rejected for PR #{pr}:"
    )?;
    writeln!(stdout, "  reason: {error}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "This usually means GitHub branch protection requires checks Shipyard"
    )?;
    writeln!(
        stdout,
        "doesn't supervise (e.g. GHA-hosted Linux/Windows still in_progress). Either:"
    )?;
    writeln!(
        stdout,
        "  * re-run `shipyard ship --pr {pr}` after the remaining checks complete, or"
    )?;
    writeln!(
        stdout,
        "  * enable native auto-merge: `gh pr merge {pr} --squash --auto`"
    )?;
    Ok(())
}

fn render_validation_failed<W: Write>(
    stdout: &mut W,
    pr: u64,
    diagnostics: &[RenderedDiagnostics],
) -> std::io::Result<()> {
    writeln!(stdout, "\u{2717} Validation failed. PR #{pr} not merged.")?;
    if diagnostics.is_empty() {
        writeln!(
            stdout,
            "  (no per-target diagnostics; rerun with --json for raw run state)"
        )?;
        return Ok(());
    }
    for (idx, entry) in diagnostics.iter().enumerate() {
        if idx > 0 {
            writeln!(stdout)?;
        }
        match entry.kind {
            FailureKind::Cancelled => {
                writeln!(
                    stdout,
                    "  \u{223C} Validation cancelled (concurrency-replaced or skipped); not a failure"
                )?;
                writeln!(stdout, "    Target:  {}", entry.target.target_name)?;
            }
            FailureKind::TimedOut => {
                writeln!(
                    stdout,
                    "  \u{2717} Validation timed out{}",
                    entry
                        .target
                        .error_message
                        .as_deref()
                        .map(|m| format!(" — {m}"))
                        .unwrap_or_default(),
                )?;
                writeln!(stdout, "    Target:  {}", entry.target.target_name)?;
            }
            FailureKind::Failed => {
                let provider = entry
                    .target
                    .provider
                    .as_deref()
                    .map(|p| format!(" (cloud={p})"))
                    .unwrap_or_default();
                writeln!(
                    stdout,
                    "    Target:  {}{provider}",
                    entry.target.target_name
                )?;
                if let Some(details) = entry.details.as_ref() {
                    if let Some(job) = details.job.as_ref() {
                        writeln!(stdout, "    Job:     {}", job.name)?;
                        if !job.html_url.is_empty() {
                            writeln!(stdout, "    URL:     {}", job.html_url)?;
                        }
                        if let Some(step) = job.failed_step.as_deref() {
                            writeln!(stdout, "    Step:    \"{step}\"")?;
                        }
                    } else if let Some(run_id) = details.run_id {
                        writeln!(
                            stdout,
                            "    Run ID:  {run_id} (failed-job lookup unavailable)"
                        )?;
                    }
                    if !details.failure_summary.is_empty() {
                        writeln!(stdout, "    Tests:")?;
                        for line in &details.failure_summary {
                            writeln!(stdout, "      {line}")?;
                        }
                        if details.failure_summary_truncated {
                            writeln!(stdout, "      (truncated; see job log for full list)")?;
                        }
                    } else if details.log_tail.is_some() {
                        writeln!(stdout, "    Tests:   (no recognised footer; see job URL)")?;
                    }
                } else if let Some(message) = entry.target.error_message.as_deref() {
                    writeln!(stdout, "    Error:   {message}")?;
                }
            }
        }
    }
    writeln!(
        stdout,
        "    Action:  run `shipyard watch --pr {pr}` to follow recovery, or push fix."
    )?;
    Ok(())
}

fn failure_kind_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Cancelled => "cancelled",
        FailureKind::TimedOut => "timed_out",
        FailureKind::Failed => "failed",
    }
}

fn git_repo_slug(cwd: &Path) -> Option<String> {
    let remote = git_optional(cwd, &["remote", "get-url", "origin"])?;
    parse_github_repo_slug(&remote)
}

fn git_required(cwd: &Path, args: &[&str]) -> Result<String, CliFailure> {
    git_optional(cwd, args).ok_or_else(|| CliFailure::new(1, "Not in a git repository"))
}

fn git_optional(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = crate::supervised::git_supervised()
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn fields(items: impl IntoIterator<Item = (&'static str, Value)>) -> BTreeMap<String, Value> {
    items
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::process::{ExitCode, Stdio};

    use toml::Table;

    use super::{ShipCommandArgs, ShipRenderState, render_green_not_merged, ship_command};
    use crate::app::cli::MergeResult;
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use crate::identity::RuntimeMode;
    use crate::paths::RuntimePaths;

    /// Issue #301 (2/3): the render must surface the underlying merge
    /// error verbatim and point the user at the two unblocks
    /// (re-ship after checks complete, OR `gh pr merge --auto`).
    /// It must NOT claim "all green" — when this branch fires, Shipyard
    /// only validated local lanes; GitHub branch protection rejected
    /// the merge because GHA-hosted checks were still in flight.
    #[test]
    fn render_green_not_merged_surfaces_error_and_unblock_options() {
        let mut buf = Vec::<u8>::new();
        let err = "GraphQL: Pull request is not mergeable: Base branch was modified.";
        render_green_not_merged(&mut buf, 2020, err).expect("render");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(
            out.contains("PR #2020"),
            "must name the PR number; got:\n{out}"
        );
        assert!(
            out.contains(err),
            "must surface the merge error verbatim; got:\n{out}"
        );
        assert!(
            !out.contains("All green"),
            "must NOT claim 'all green' when the merge attempt was rejected; got:\n{out}"
        );
        assert!(
            out.contains("shipyard ship --pr 2020"),
            "must hint at re-running shipyard ship; got:\n{out}"
        );
        assert!(
            out.contains("gh pr merge 2020 --squash --auto"),
            "must hint at native auto-merge as the second option; got:\n{out}"
        );
    }

    #[test]
    fn ship_render_state_only_merged_returns_true_for_merged() {
        assert!(ShipRenderState::Merged.merged());
        assert!(!ShipRenderState::ValidationFailed.merged());
        assert!(!ShipRenderState::GreenNotMerged("err".to_owned()).merged());
    }

    fn git(args: &[&str], cwd: &std::path::Path) {
        let status = crate::supervised::git_supervised()
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git command should run");
        assert!(status.success(), "git command failed: {args:?}");
    }

    /// Capture a git command's trimmed stdout (e.g. `rev-parse HEAD`) so a
    /// test can pin the issue #321 merge preflight's live-head snapshot to
    /// the seeded repo's real HEAD SHA.
    fn git_capture(args: &[&str], cwd: &std::path::Path) -> String {
        let output = crate::supervised::git_supervised()
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git command should run");
        assert!(output.status.success(), "git command failed: {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn seed_repo(repo: &std::path::Path) {
        std::fs::create_dir_all(repo).expect("repo dir");
        git(&["init", "--quiet", "--initial-branch=main"], repo);
        std::fs::write(repo.join("README.md"), "seed\n").expect("readme");
        git(&["add", "."], repo);
        git(&["commit", "-q", "-m", "seed"], repo);
        git(
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/danielraffel/pulp.git",
            ],
            repo,
        );
        git(&["checkout", "-q", "-b", "feature/test"], repo);
    }

    #[cfg(unix)]
    fn seed_repo_with_local_origin(repo: &std::path::Path, remote: &std::path::Path) {
        std::fs::create_dir_all(repo).expect("repo dir");
        std::fs::create_dir_all(remote).expect("remote dir");
        git(&["init", "--quiet", "--bare"], remote);
        git(&["init", "--quiet", "--initial-branch=main"], repo);
        std::fs::write(repo.join("README.md"), "seed\n").expect("readme");
        git(&["add", "."], repo);
        git(&["commit", "-q", "-m", "Seed repo"], repo);
        git(
            &["remote", "add", "origin", remote.to_str().expect("remote")],
            repo,
        );
        git(&["push", "-u", "origin", "main"], repo);
        git(&["checkout", "-q", "-b", "feature/test"], repo);
    }

    #[cfg(unix)]
    fn fake_gh(path: &std::path::Path, script_body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{script_body}\n")).expect("fake gh");
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod");
    }

    fn loaded_config(root: &std::path::Path) -> LoadedConfig {
        let data = r#"
            [validation.default]
            command = "rustc --version"

            [targets.mac]
            backend = "local"
            platform = "macos-arm64"
        "#
        .parse::<Table>()
        .expect("config TOML");
        LoadedConfig {
            data,
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn unreachable_ssh_config(root: &std::path::Path) -> LoadedConfig {
        let data = r#"
            [validation.default]
            command = "make test"

            [targets.linux]
            backend = "ssh"
            platform = "linux-x64"
            repo_path = "~/repo"
        "#
        .parse::<Table>()
        .expect("config TOML");
        LoadedConfig {
            data,
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn local_and_unreachable_config(root: &std::path::Path) -> LoadedConfig {
        let data = r#"
            [validation.default]
            command = "rustc --version"

            [targets.mac]
            backend = "local"
            platform = "macos-arm64"

            [targets.linux]
            backend = "ssh"
            platform = "linux-x64"
            repo_path = "~/repo"
        "#
        .parse::<Table>()
        .expect("config TOML");
        LoadedConfig {
            data,
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    #[test]
    fn auto_create_base_default_matches_python_patterns() {
        assert!(super::should_auto_create_base("develop/next", None));
        assert!(super::should_auto_create_base("release/1.2", None));
        assert!(!super::should_auto_create_base("develop", None));
        assert!(!super::should_auto_create_base("main", None));
        assert!(super::should_auto_create_base("main", Some(true)));
        assert!(!super::should_auto_create_base("develop/next", Some(false)));
    }

    #[test]
    fn ship_command_runs_local_target_merges_and_archives_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        // The issue #321 merge preflight verifies the live PR head matches
        // the validated SHA. Pin the live head to the seeded repo's real HEAD
        // so the happy-path merge proceeds.
        let head = git_capture(&["rev-parse", "HEAD"], &repo);
        let snapshot = temp.path().join("pr.json");
        std::fs::write(&snapshot, format!(r#"{{"headRefOid":"{head}"}}"#))
            .expect("write snapshot");
        let mut stdout = Vec::new();

        let code = ship_command(
            ShipCommandArgs {
                pr: Some(42),
                base: "main".to_owned(),
                auto_create_base: None,
                no_warm: true,
                resume_from: None,
                merge_command: None,
                merge_result: Some(MergeResult::Success),
                gh_command: None,
                pr_snapshot_file: Some(snapshot),
                allow_unreachable_targets: false,
                skip_targets: Vec::new(),
            },
            &loaded_config(temp.path()),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect("ship command");

        assert_eq!(code, ExitCode::SUCCESS);
        let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(output["command"], "ship");
        assert_eq!(output["pr"], 42);
        assert_eq!(output["merged"], true);
        assert_eq!(output["run"]["overall"], "pass");
        assert_eq!(output["ship_state"]["repo"], "danielraffel/pulp");
        assert_eq!(output["ship_state"]["evidence_snapshot"]["mac"], "pass");
        assert!(!paths.state_dir.join("ship").join("42.json").exists());
        assert_eq!(
            std::fs::read_dir(paths.state_dir.join("ship").join("archive"))
                .expect("archive")
                .count(),
            1
        );
    }

    // Regression coverage for Shipyard issue #296. The synthetic
    // `MergeResult::Failure` injects `Err("simulated merge failure")` in
    // `merge_pr`. `execute_auto_merge` then evaluates
    // `merge_error_confirms_merged(error) || pr_is_merged(...)` as a
    // "did the merge actually succeed despite the error?" escape hatch.
    // `pr_is_merged` shells out to `gh pr view <pr> --json state` against
    // the temp repo's `origin` remote (https://github.com/danielraffel/pulp).
    // PR #43 *is* merged in that upstream repo, so on hosts with a fresh
    // GraphQL budget `pr_is_merged` returns true and the failure path
    // archives the state and returns `Merged` — producing the observed
    // `merged: true`. Pinning `--pr-snapshot-file` (via the new
    // `pr_snapshot_file` field on `ShipCommandArgs`) keeps `pr_is_merged`
    // offline and deterministic.
    #[test]
    fn ship_command_green_merge_failure_keeps_active_state_and_exits_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        // `state:OPEN` keeps the failure-path `pr_is_merged` escape hatch
        // closed; `headRefOid` matching the seeded HEAD lets the issue #321
        // preflight pass so the injected `MergeResult::Failure` is the thing
        // under test.
        let head = git_capture(&["rev-parse", "HEAD"], &repo);
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            format!(r#"{{"state":"OPEN","headRefOid":"{head}"}}"#),
        )
        .expect("write snapshot");
        let mut stdout = Vec::new();

        let code = ship_command(
            ShipCommandArgs {
                pr: Some(43),
                base: "main".to_owned(),
                auto_create_base: None,
                no_warm: true,
                resume_from: None,
                merge_command: None,
                merge_result: Some(MergeResult::Failure),
                gh_command: None,
                pr_snapshot_file: Some(snapshot),
                allow_unreachable_targets: false,
                skip_targets: Vec::new(),
            },
            &loaded_config(temp.path()),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect("ship command");

        assert_eq!(code, ExitCode::SUCCESS);
        let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(output["merged"], false);
        assert_eq!(output["run"]["overall"], "pass");
        assert!(paths.state_dir.join("ship").join("43.json").exists());
        assert_eq!(
            std::fs::read_dir(paths.state_dir.join("ship").join("archive"))
                .expect("archive")
                .count(),
            0
        );
    }

    #[test]
    fn ship_command_preflight_failure_happens_before_state_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut stdout = Vec::new();

        let error = ship_command(
            ShipCommandArgs {
                pr: Some(44),
                base: "main".to_owned(),
                auto_create_base: None,
                no_warm: true,
                resume_from: None,
                merge_command: None,
                merge_result: Some(MergeResult::Success),
                gh_command: None,
                pr_snapshot_file: None,
                allow_unreachable_targets: false,
                skip_targets: Vec::new(),
            },
            &unreachable_ssh_config(temp.path()),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect_err("preflight should fail");

        assert_eq!(error.code, crate::preflight::EXIT_BACKEND_UNREACHABLE);
        assert!(
            error
                .message
                .contains("Target 'linux' (ssh) is unreachable.")
        );
        assert!(error.message.contains("target has no host configured"));
        assert!(stdout.is_empty());
        assert!(!paths.state_dir.join("queue.json").exists());
        assert!(!paths.state_dir.join("ship").exists());
    }

    #[test]
    fn ship_command_skip_target_excludes_unreachable_target_before_preflight() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut stdout = Vec::new();

        let code = ship_command(
            ShipCommandArgs {
                pr: Some(45),
                base: "main".to_owned(),
                auto_create_base: None,
                no_warm: true,
                resume_from: None,
                merge_command: None,
                merge_result: Some(MergeResult::Success),
                gh_command: None,
                pr_snapshot_file: None,
                allow_unreachable_targets: false,
                skip_targets: vec!["linux".to_owned()],
            },
            &local_and_unreachable_config(temp.path()),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect("ship command");

        assert_eq!(code, ExitCode::SUCCESS);
        let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        let evidence = output["ship_state"]["evidence_snapshot"]
            .as_object()
            .expect("evidence");
        assert_eq!(evidence["mac"], "pass");
        assert!(!evidence.contains_key("linux"));
    }

    #[test]
    #[cfg(unix)]
    fn ship_command_without_pr_finds_existing_pr_after_preflight_and_push() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        seed_repo_with_local_origin(&repo, &remote);
        let gh = temp.path().join("gh");
        let gh_log = temp.path().join("gh.log");
        fake_gh(
            &gh,
            &format!(
                r#"
echo "$@" >> "{}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":88,"url":"https://github.com/o/r/pull/88","title":"Existing PR","state":"OPEN","headRefName":"feature/test","baseRefName":"main"}}]'
  exit 0
fi
echo "unexpected gh args: $@" >&2
exit 2
"#,
                gh_log.display()
            ),
        );
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut stdout = Vec::new();

        let code = ship_command(
            ShipCommandArgs {
                pr: None,
                base: "main".to_owned(),
                auto_create_base: None,
                no_warm: true,
                resume_from: None,
                merge_command: None,
                merge_result: Some(MergeResult::Success),
                gh_command: Some(gh),
                pr_snapshot_file: None,
                allow_unreachable_targets: false,
                skip_targets: Vec::new(),
            },
            &loaded_config(temp.path()),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect("ship command");

        assert_eq!(code, ExitCode::SUCCESS);
        let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(output["pr"], 88);
        assert_eq!(
            output["ship_state"]["pr_url"],
            "https://github.com/o/r/pull/88"
        );
        assert_eq!(output["ship_state"]["pr_title"], "Existing PR");
        assert!(
            String::from_utf8_lossy(
                &crate::supervised::git_supervised()
                    .args(["show-ref", "refs/heads/feature/test"])
                    .current_dir(&remote)
                    .output()
                    .expect("show-ref")
                    .stdout
            )
            .contains("refs/heads/feature/test")
        );
        assert!(
            std::fs::read_to_string(gh_log)
                .expect("gh log")
                .contains("pr list")
        );
    }

    #[test]
    #[cfg(unix)]
    fn ship_command_without_pr_creates_pr_when_none_exists() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        seed_repo_with_local_origin(&repo, &remote);
        std::fs::write(repo.join("feature.txt"), "feature\n").expect("feature");
        git(&["add", "."], &repo);
        git(
            &[
                "commit",
                "-q",
                "-m",
                "Add autopilot",
                "-m",
                "Context\n\nLane-Policy: mac=advisory",
            ],
            &repo,
        );
        let gh = temp.path().join("gh");
        let gh_log = temp.path().join("gh.log");
        fake_gh(
            &gh,
            &format!(
                r#"
echo "$@" >> "{}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  echo 'https://github.com/o/r/pull/89'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"number":89,"url":"https://github.com/o/r/pull/89","title":"Add autopilot","state":"OPEN","headRefName":"feature/test","baseRefName":"develop/test"}}'
  exit 0
fi
echo "unexpected gh args: $@" >&2
exit 2
"#,
                gh_log.display()
            ),
        );
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut stdout = Vec::new();

        let code = ship_command(
            ShipCommandArgs {
                pr: None,
                base: "develop/test".to_owned(),
                auto_create_base: None,
                no_warm: true,
                resume_from: None,
                merge_command: None,
                merge_result: Some(MergeResult::Success),
                gh_command: Some(gh),
                pr_snapshot_file: None,
                allow_unreachable_targets: false,
                skip_targets: Vec::new(),
            },
            &loaded_config(temp.path()),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect("ship command");

        assert_eq!(code, ExitCode::SUCCESS);
        let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(output["pr"], 89);
        assert_eq!(output["ship_state"]["base_branch"], "develop/test");
        assert_eq!(output["ship_state"]["pr_title"], "Add autopilot");
        assert!(
            String::from_utf8_lossy(
                &crate::supervised::git_supervised()
                    .args(["show-ref", "refs/heads/develop/test"])
                    .current_dir(&remote)
                    .output()
                    .expect("show-ref")
                    .stdout
            )
            .contains("refs/heads/develop/test")
        );
        let log = std::fs::read_to_string(gh_log).expect("gh log");
        assert!(log.contains("pr list"));
        assert!(log.contains("pr create"));
        assert!(log.contains("pr view"));
        assert!(log.contains("Lane-Policy: mac=advisory"));
        assert!(log.contains("## Advisory lanes"));
        assert!(log.contains("`mac` (overridden via Lane-Policy trailer)"));
    }
}
