use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

use super::daemon_cmd::ensure_execution_daemon;
use super::{
    CliFailure, SHIP_EXIT_MERGE_CLIENT_DEFECT, SHIP_EXIT_VALIDATION_STATE_MISSING,
    auto_merge_cmd::{
        AutoMergeOutcome, AutoMergeRequest, execute_auto_merge, is_graphql_malformed_query_error,
        supervise_merge_queue,
    },
    cli::{MergeMethod, MergeResult},
    merge_steward_cmd::{
        StewardHandoffArgs, steward_handoff_command, steward_handoff_transfer_report,
    },
    wait_cmd::parse_github_repo_slug,
};
use crate::auto_rescue::{
    WedgeClass, WedgeInputs, classify_wedge, sha_matches, validated_green_contexts,
};
use crate::config::LoadedConfig;
use crate::diagnostics::{
    FailureDiagnostics, FailureKind, GhDiagnosticsFetcher, fetch_failed_job_diagnostics,
    select_parser,
};
use crate::evidence::EvidenceStore;
use crate::executor::dispatch::{ExecutorDispatcher, ResolvedTarget, resolve_targets};
use crate::governance::{GovernanceGh, put_branch_protection, resolve_branch_rules};
use crate::identity::RuntimeMode;
use crate::job::{Job, Priority, TargetResult, TargetStatus, ValidationMode};
use crate::lane_policy::{LanePolicy, resolve_lane_policy};
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::pr::{
    PrInfo, create_pr, find_pr_for_branch, get_pr_checkout_status, push_branch_with_env,
};
use crate::pr_text::{compose_pr_body_with_policy, compose_pr_title};
use crate::preflight::{
    EXIT_BACKEND_UNREACHABLE, EXIT_FLEET_EPOCH_DRIFT, EXIT_HOST_UNHEALTHY, ShipPreflightError,
    ShipPreflightOptions, collect_ship_preflight_with_options,
};
use crate::prepared_state::PreparedStateStore;
use crate::queue::Queue;
use crate::reconcile::fetch_head_and_status_check_rollup_with_cwd;
use crate::ship::{
    ShipExecutionRequest, ShipStores, drain_or_wait_ship, submit_ship, submit_ship_daemon,
    validate_ship_state_for_submission,
};
use crate::ship_state::ShipStateStore;
use crate::warm_pool::{WarmPool, default_pool_path};

// A CLI argument bag: one bool per user-facing flag is the shape the
// command line already has, and grouping them into sub-structs would
// only move the flags further from the flags they mirror.
#[allow(clippy::struct_excessive_bools)]
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
    /// Proceed even when this host has not converged to the declared fleet epoch.
    pub(super) allow_fleet_epoch_drift: bool,
    pub(super) skip_targets: Vec<String>,
    /// Adopt the current head SHA when recorded ship-state drifted (amend /
    /// force-push), clearing prior evidence so the new head re-validates
    /// instead of dead-ending on `ShaDrift`. See Shipyard #346.
    pub(super) adopt_head: bool,
    pub(super) steward_handoff: Option<ShipStewardHandoff>,
    /// Run configured PR provenance before any durable steward receipt or
    /// validation dispatch. Enabled by `shipyard pr`, never by an explicit
    /// `ship --pr` recovery that lacks the submitting session's context.
    pub(super) invocation: ShipInvocation,
    pub(super) foreground: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShipInvocation {
    Direct,
    PrCommand,
}

pub(super) struct ShipStewardHandoff {
    pub(super) workstream_id: Option<String>,
    pub(super) context_url: Option<String>,
    pub(super) launch_profile: Option<std::path::PathBuf>,
}

mod changed_surface_execution;
mod metadata_authority;
mod prepush_changed_surface;
mod provenance;
use changed_surface_execution::apply_changed_surface_execution;
use provenance::{AppliedStewardHandoff, apply_requested_steward_handoff, run_pr_provenance_hook};

#[allow(clippy::too_many_lines)]
pub(super) fn ship_command<W: Write>(
    args: ShipCommandArgs,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let daemon_owned = !args.foreground && cfg!(unix);
    validate_daemon_ship_submission(
        daemon_owned,
        args.merge_command.is_some()
            || args.merge_result.is_some()
            || args.pr_snapshot_file.is_some(),
        config.get_str("github.auth.source"),
    )?;
    let branch = git_required(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let sha = git_required(cwd, &["rev-parse", "HEAD"])?;
    let commit_subject =
        git_optional(cwd, &["log", "-1", "--format=%s", "HEAD"]).unwrap_or_default();
    let repo = git_repo_slug(cwd).unwrap_or_default();
    let defer_native_preflight = match crate::metadata_authority::trusted_policy(config, &repo) {
        Ok(policy) => policy.is_some(),
        Err(error) => {
            let _ = crate::writer_domain_lease::write_stderr(format_args!(
                "warning: metadata authority policy unavailable; preserving ordinary full validation: {error}"
            ));
            false
        }
    };
    let preflight_dispatcher = ExecutorDispatcher::new(None);
    let mut targets = prepare_ship_targets(
        config,
        cwd,
        runtime_paths,
        &preflight_dispatcher,
        &args,
        json_mode,
        stdout,
        !defer_native_preflight,
    )?;
    if should_auto_create_base(&args.base, args.auto_create_base) {
        maybe_auto_create_base_branch(cwd, &args.base, config, args.gh_command.as_deref());
    }
    let lane_policy = resolve_lane_policy(config, cwd);
    let prepush_enabled = args.pr.is_none() && prepush_changed_surface::shadow_enabled(config)?;
    let prepush_base = if prepush_enabled {
        match find_pr_for_branch(config, cwd, args.gh_command.as_deref(), &branch) {
            Ok(Some(info)) => Some(info.base),
            Ok(None) => Some(args.base.clone()),
            Err(error) => {
                let _ = crate::writer_domain_lease::write_stderr(format_args!(
                    "warning: existing PR base unavailable; declining pre-push changed-surface optimization: {error}"
                ));
                None
            }
        }
    } else {
        None
    };
    let mut prospective_push = if prepush_enabled {
        prepush_base.as_deref().map_or(Ok(None), |base| {
            prepush_changed_surface::prepare(
                config,
                cwd,
                &runtime_paths.state_dir,
                &repo,
                base,
                &branch,
                &targets,
            )
        })?
    } else {
        None
    };
    let pr_context = resolve_pr_context(
        config,
        &args,
        cwd,
        CheckoutIdentity {
            repo: &repo,
            branch: &branch,
            head: &sha,
        },
        &lane_policy,
        prospective_push.as_mut(),
    )?;
    if let Some(prospective) = prospective_push.as_ref()
        && let Err(error) = prepush_changed_surface::verify_after_push(
            prospective,
            config,
            cwd,
            &runtime_paths.state_dir,
            &repo,
            pr_context.number,
            &branch,
        )
    {
        // A pre-push optimization can never prevent or weaken the ordinary
        // downstream full path. Identity/result ambiguity merely declines its
        // future dedupe hint.
        if json_mode {
            let _ = crate::writer_domain_lease::write_stderr(format_args!(
                "warning: pre-push changed-surface receipt not reusable: {}",
                error.message
            ));
        } else {
            writeln!(
                stdout,
                "warning: pre-push changed-surface receipt not reusable: {}",
                error.message
            )
            .map_err(|write_error| CliFailure::new(1, write_error.to_string()))?;
        }
    }
    if args.invocation == ShipInvocation::PrCommand {
        run_pr_provenance_hook(
            config,
            cwd,
            stdout,
            &repo,
            &branch,
            &pr_context.base_branch,
            &sha,
            &pr_context,
        )?;
    }
    let steward_handoff = apply_requested_steward_handoff(
        args.steward_handoff.as_ref(),
        &repo,
        &sha,
        &pr_context,
        config,
        cwd,
        runtime_paths,
        json_mode,
        stdout,
    )?;

    let metadata_authority_receipt = metadata_authority::observe_and_authorize(
        config,
        cwd,
        &runtime_paths.state_dir,
        &repo,
        pr_context.number,
        &sha,
        &targets,
    )?;
    if metadata_authority_receipt.is_some() {
        targets.clear();
    } else {
        if defer_native_preflight {
            targets = prepare_ship_targets(
                config,
                cwd,
                runtime_paths,
                &preflight_dispatcher,
                &args,
                json_mode,
                stdout,
                true,
            )?;
        }
        apply_changed_surface_execution(
            config,
            cwd,
            &runtime_paths.state_dir,
            &repo,
            Some(pr_context.number),
            args.resume_from.as_deref(),
            &mut targets,
        )?;
    }

    let mut queue = Queue::new(runtime_paths.state_dir.clone())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let evidence = EvidenceStore::new(runtime_paths.state_dir.join("evidence"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let ship_state = ShipStateStore::new(runtime_paths.state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let prepared = PreparedStateStore::new(runtime_paths.state_dir.join("prepared"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let warm_pool = WarmPool::new(default_pool_path(&runtime_paths.state_dir));
    let dispatcher = ExecutorDispatcher::new_with_state_dir_and_log_retention(
        Some(prepared),
        &runtime_paths.state_dir,
        crate::log_retention::LogRetentionPolicy::from_config(config),
    );
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
        adopt_head: args.adopt_head,
        pr_snapshot_file: args.pr_snapshot_file.clone(),
        metadata_authority_receipt,
        targets,
    };

    // Fail a known stale validation identity synchronously. The worker repeats
    // this under the per-PR lock, but deferring the first check until execution
    // can waste minutes behind unrelated repositories before the request is
    // inevitably cancelled for missing explicit `--adopt-head` authority.
    validate_ship_state_for_submission(&request, &ship_state)
        .map_err(|error| CliFailure::new(2, error.to_string()))?;

    if daemon_owned
        && crate::queue_request::ExecutionProvenance::capture_with_config(
            cwd,
            Some(&request.repo),
            &request.sha,
            config,
        )
        .is_none()
    {
        return Err(CliFailure::new(
            2,
            "could not capture exact repository, origin, HEAD, and tree provenance for daemon ownership",
        ));
    }

    if daemon_owned {
        ensure_execution_daemon(
            if runtime_paths.mode == RuntimeMode::Isolated.as_str() {
                RuntimeMode::Isolated
            } else {
                RuntimeMode::Shipyard
            },
            runtime_paths,
            vec![request.repo.clone()],
        )?;
    }
    let job = if daemon_owned {
        submit_ship_daemon(&request, &mut queue, cwd, &runtime_paths.state_dir, config)
    } else {
        submit_ship(&request, &mut queue, cwd, &runtime_paths.state_dir)
    }
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if daemon_owned {
        if json_mode {
            write_json_envelope(
                stdout,
                "ship",
                fields([
                    ("ship", job.to_json_value()),
                    ("pr", Value::from(pr_context.number)),
                ]),
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else {
            writeln!(
                stdout,
                "Queued {} for PR #{}. The Shipyard daemon owns execution.",
                job.id, pr_context.number
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        return Ok(ExitCode::SUCCESS);
    }
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
            config,
        },
        &dispatcher,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;

    let render_state = post_run_merge_state(
        pr_context.number,
        cwd,
        &ship_state,
        config,
        if runtime_paths.mode == RuntimeMode::Isolated.as_str() {
            RuntimeMode::Isolated
        } else {
            RuntimeMode::Shipyard
        },
        &request.repo,
        outcome.job.passed(),
        &outcome.ship_state,
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
            &render_state,
            &diagnostics,
            steward_handoff.as_ref(),
        )?;
    } else {
        render_human(stdout, pr_context.number, &render_state, &diagnostics)?;
    }
    Ok(render_state.exit_code())
}

fn validate_daemon_ship_submission(
    daemon_owned: bool,
    has_test_merge_override: bool,
    auth_source: Option<&str>,
) -> Result<(), CliFailure> {
    if !daemon_owned {
        return Ok(());
    }
    if has_test_merge_override {
        return Err(CliFailure::new(
            2,
            "test merge overrides require --foreground",
        ));
    }
    if auth_source != Some("command") {
        return Err(CliFailure::new(
            2,
            "daemon-owned ship requires github.auth.source = command so an existing daemon can refresh credentials; env and ambient gh auth are forbidden",
        ));
    }
    Ok(())
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

#[allow(clippy::too_many_arguments)] // Explicit CLI/preflight dependencies keep authority testable.
fn prepare_ship_targets<W: Write>(
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    preflight_dispatcher: &ExecutorDispatcher,
    args: &ShipCommandArgs,
    json_mode: bool,
    stdout: &mut W,
    perform_preflight: bool,
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
    if !perform_preflight {
        return Ok(targets);
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
            allow_fleet_epoch_drift: args.allow_fleet_epoch_drift,
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
        ShipPreflightError::HostUnhealthy { .. } => EXIT_HOST_UNHEALTHY,
        ShipPreflightError::FleetEpochDrift { .. } => EXIT_FLEET_EPOCH_DRIFT,
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

#[derive(Clone, Copy)]
struct CheckoutIdentity<'a> {
    repo: &'a str,
    branch: &'a str,
    head: &'a str,
}

fn resolve_pr_context(
    config: &LoadedConfig,
    args: &ShipCommandArgs,
    cwd: &Path,
    checkout: CheckoutIdentity<'_>,
    lane_policy: &LanePolicy,
    mut prospective_push: Option<&mut prepush_changed_surface::ProspectivePush>,
) -> Result<ResolvedPrContext, CliFailure> {
    if let Some(number) = args.pr {
        if let Some(path) = args.pr_snapshot_file.as_deref() {
            let value: Value = std::fs::read_to_string(path)
                .map_err(|error| CliFailure::new(1, format!("failed to read PR snapshot: {error}")))
                .and_then(|payload| {
                    serde_json::from_str(&payload).map_err(|error| {
                        CliFailure::new(1, format!("failed to parse PR snapshot: {error}"))
                    })
                })?;
            let base_branch = value
                .get("baseRefName")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| value.pointer("/base/ref").and_then(Value::as_str))
                .unwrap_or(&args.base)
                .to_owned();
            let pr_branch = value
                .get("headRefName")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| CliFailure::new(2, "PR snapshot omitted the exact head branch"))?;
            let pr_head = value
                .get("headRefOid")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| CliFailure::new(2, "PR snapshot omitted the exact head SHA"))?;
            validate_explicit_pr_checkout(number, checkout, None, pr_branch, pr_head)?;
            return Ok(ResolvedPrContext {
                number,
                base_branch,
                pr_url: None,
                pr_title: None,
            });
        }
        let checkout_info =
            get_pr_checkout_status(config, cwd, args.gh_command.as_deref(), &number.to_string())
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        let info = checkout_info.info;
        let pr_repo = pull_request_repo_slug(&info.url).ok_or_else(|| {
            CliFailure::new(
                2,
                format!(
                    "refusing ship --pr {number}: live pull request URL does not identify an exact GitHub repository: {}",
                    info.url
                ),
            )
        })?;
        validate_explicit_pr_checkout(
            number,
            checkout,
            Some(&pr_repo),
            &info.branch,
            &checkout_info.head_sha,
        )?;
        return Ok(ResolvedPrContext {
            number,
            base_branch: info.base,
            pr_url: Some(info.url),
            pr_title: Some(info.title),
        });
    }

    let environment = prospective_push
        .as_deref_mut()
        .map_or_else(Vec::new, |push| {
            let environment = push.environment();
            push.handoff_writer_domain_to_child();
            environment
        });
    push_branch_with_env(cwd, checkout.branch, environment)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if let Some(push) = prospective_push {
        // This private state transition is the pass authority. The hook result
        // is identity telemetry only; untrusted test descendants cannot turn a
        // nonzero/aborted git push into this parent-observed state.
        push.mark_supervised_push_succeeded();
    }
    let info = find_pr_for_branch(config, cwd, args.gh_command.as_deref(), checkout.branch)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .map_or_else(
            || {
                create_current_branch_pr(
                    config,
                    cwd,
                    args.gh_command.as_deref(),
                    checkout.branch,
                    &args.base,
                    lane_policy,
                )
            },
            Ok::<PrInfo, CliFailure>,
        )?;
    if info.branch != checkout.branch {
        return Err(CliFailure::new(
            1,
            "authenticated pull-request head ref differs from the supervised branch",
        ));
    }
    Ok(ResolvedPrContext {
        number: info.number,
        base_branch: info.base,
        pr_url: Some(info.url),
        pr_title: Some(info.title),
    })
}

fn validate_explicit_pr_checkout(
    number: u64,
    checkout: CheckoutIdentity<'_>,
    pr_repo: Option<&str>,
    pr_branch: &str,
    pr_head: &str,
) -> Result<(), CliFailure> {
    let repo_matches = pr_repo.is_none_or(|value| value.eq_ignore_ascii_case(checkout.repo));
    if !checkout.repo.is_empty()
        && repo_matches
        && checkout.branch == pr_branch
        && checkout.head.eq_ignore_ascii_case(pr_head)
    {
        return Ok(());
    }

    Err(CliFailure::new(
        2,
        format!(
            "refusing ship --pr {number}: current checkout does not match live pull request; local repo {local_repo}, local branch {local_branch}, local HEAD {local_head}; PR repo {}, PR head branch {pr_branch}, PR head {pr_head}. Run this command from the exact PR worktree.",
            pr_repo.unwrap_or("<snapshot-scoped>"),
            local_repo = checkout.repo,
            local_branch = checkout.branch,
            local_head = checkout.head,
        ),
    ))
}

fn pull_request_repo_slug(url: &str) -> Option<String> {
    let path = url
        .trim()
        .trim_end_matches('/')
        .strip_prefix("https://github.com/")?;
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty()
        || repo.is_empty()
        || parts.next()? != "pull"
        || parts.next()?.parse::<u64>().is_err()
        || parts.next().is_some()
    {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

fn create_current_branch_pr(
    config: &LoadedConfig,
    cwd: &Path,
    gh_command: Option<&Path>,
    branch: &str,
    base: &str,
    lane_policy: &LanePolicy,
) -> Result<PrInfo, CliFailure> {
    create_pr(
        config,
        cwd,
        gh_command,
        branch,
        base,
        &compose_pr_title(cwd, branch),
        &compose_pr_body_with_policy(cwd, Some(lane_policy)),
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

mod post_validation;
use post_validation::{ShipRenderState, post_run_merge_state};

/// Complete the post-validation merge phase for a daemon-owned ship request.
pub(super) fn finish_background_ship(
    request: &ShipExecutionRequest,
    job: &Job,
    mode: RuntimeMode,
    global_dir: &Path,
    state_dir: &Path,
) -> Result<
    (
        ExitCode,
        Option<crate::ship_state::ShipState>,
        crate::queue_request::QueuedShipDisposition,
    ),
    CliFailure,
> {
    let request_store = crate::queue_request::QueueRequestStore::new(state_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let envelope = request_store
        .load(&job.id)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .ok_or_else(|| CliFailure::new(1, "ship request disappeared before merge"))?;
    let config =
        LoadedConfig::load_from_cwd_with_global_dir(mode, &envelope.cwd, global_dir.to_path_buf())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let provenance = envelope
        .provenance
        .as_ref()
        .ok_or_else(|| CliFailure::new(1, "ship request lacks unattended provenance"))?;
    provenance
        .validate_with_config(&envelope.cwd, &config)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let ship_state = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    // Capture the validated state before merge supervision can archive it. A
    // missing state is itself a typed post-validation result, not a validation
    // failure: `post_run_merge_state` returns
    // `GreenValidationStateMissing`, while the queue worker preserves the
    // already-completed validation job through its state-independent outcome
    // fallback.
    let terminal_state = ship_state.get_scoped(&request.repo, request.pr);
    let validated_state = crate::ship::validation_proof_state(request, job, terminal_state.clone());
    let state = post_run_merge_state(
        request.pr,
        &envelope.cwd,
        &ship_state,
        &config,
        mode,
        &request.repo,
        job.passed(),
        &validated_state,
        None,
        None,
        request.pr_snapshot_file.clone(),
    )?;
    Ok((
        state.exit_code(),
        terminal_state,
        state.queued_disposition(),
    ))
}

mod render;
use render::{render_human, render_json};

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
#[path = "ship_cmd/command_tests.rs"]
mod command_tests;
#[cfg(test)]
#[path = "ship_cmd/provenance_tests.rs"]
mod provenance_tests;
#[cfg(test)]
#[path = "ship_cmd/render_tests.rs"]
mod render_tests;
#[cfg(test)]
#[path = "ship_cmd/test_support.rs"]
mod test_support;
