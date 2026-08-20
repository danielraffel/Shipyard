use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::{Value, json};

use super::{
    CliFailure, SHIP_EXIT_MERGE_CLIENT_DEFECT,
    auto_merge_cmd::{
        AutoMergeOutcome, AutoMergeRequest, execute_auto_merge, is_graphql_malformed_query_error,
        supervise_merge_queue,
    },
    cli::{MergeMethod, MergeResult},
    merge_steward_cmd::{StewardHandoffArgs, steward_handoff_command},
    wait_cmd::parse_github_repo_slug,
};
use crate::auto_rescue::{
    WedgeClass, WedgeInputs, classify_wedge, sha_matches, validated_green_contexts,
};
use crate::cloud::GitHubActions;
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
use crate::pr::{PrInfo, create_pr, find_pr_for_branch, get_pr_status, push_branch};
use crate::pr_text::{compose_pr_body_with_policy, compose_pr_title};
use crate::preflight::{
    EXIT_BACKEND_UNREACHABLE, EXIT_FLEET_EPOCH_DRIFT, EXIT_HOST_UNHEALTHY, ShipPreflightError,
    ShipPreflightOptions, collect_ship_preflight_with_options,
};
use crate::prepared_state::PreparedStateStore;
use crate::queue::Queue;
use crate::reconcile::fetch_head_and_status_check_rollup_with_cwd;
use crate::ship::{ShipExecutionRequest, ShipStores, drain_or_wait_ship, submit_ship};
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShipInvocation {
    Direct,
    PrCommand,
}

pub(super) struct ShipStewardHandoff {
    pub(super) workstream_id: Option<String>,
    pub(super) context_url: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct AppliedStewardHandoff {
    pub(super) workstream_id: String,
    pub(super) context_url: Option<String>,
    pub(super) head: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PrProvenanceHook {
    command: Vec<String>,
    required: bool,
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
    ship_command_with_transition(args, config, cwd, runtime_paths, json_mode, stdout, None)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn ship_command_with_transition<W: Write>(
    args: ShipCommandArgs,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json_mode: bool,
    stdout: &mut W,
    transition_guard: Option<super::pr_invocation::PrInvocationTransitionGuard>,
) -> Result<ExitCode, CliFailure> {
    let terminal_steward_handoff = is_terminal_steward_handoff(&args);
    let preflight_dispatcher = ExecutorDispatcher::new(None);
    let targets = if terminal_steward_handoff {
        None
    } else {
        Some(prepare_ship_targets(
            config,
            cwd,
            runtime_paths,
            &preflight_dispatcher,
            &args,
            json_mode,
            stdout,
        )?)
    };

    let branch = git_required(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let sha = git_required(cwd, &["rev-parse", "HEAD"])?;
    let commit_subject =
        git_optional(cwd, &["log", "-1", "--format=%s", "HEAD"]).unwrap_or_default();
    let repo = git_repo_slug(cwd).unwrap_or_default();
    if should_auto_create_base(&args.base, args.auto_create_base) {
        maybe_auto_create_base_branch(cwd, &args.base, config, args.gh_command.as_deref());
    }
    let lane_policy = resolve_lane_policy(config, cwd);
    let pr_context = resolve_pr_context(config, &args, cwd, &branch, &lane_policy)?;
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
        json_mode,
        stdout,
    )?;
    if terminal_steward_handoff {
        let receipt = steward_handoff.as_ref().ok_or_else(|| {
            CliFailure::new(
                1,
                "steward handoff was requested but no receipt was produced",
            )
        })?;
        render_terminal_steward_handoff(stdout, &repo, pr_context.number, receipt, json_mode)?;
        return Ok(ExitCode::SUCCESS);
    }
    // `shipyard pr` needs the branch fence through push/PR resolution, but the
    // traditional local-validation path must not monopolize it for a long
    // queue drain. Exact-head queue ownership takes over from this boundary.
    drop(transition_guard);
    let targets = targets.expect("non-terminal ship path prepares validation targets");

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
        adopt_head: args.adopt_head,
        pr_snapshot_file: args.pr_snapshot_file.clone(),
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

fn is_terminal_steward_handoff(args: &ShipCommandArgs) -> bool {
    args.invocation == ShipInvocation::PrCommand && args.steward_handoff.is_some()
}

pub(super) fn render_terminal_steward_handoff<W: Write>(
    stdout: &mut W,
    repo: &str,
    pr: u64,
    receipt: &AppliedStewardHandoff,
    json_mode: bool,
) -> Result<(), CliFailure> {
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("status".to_owned(), Value::from("handed_off"));
        data.insert("repo".to_owned(), Value::from(repo));
        data.insert("pr".to_owned(), Value::from(pr));
        data.insert("head_sha".to_owned(), Value::from(receipt.head.clone()));
        data.insert(
            "workstream_id".to_owned(),
            Value::from(receipt.workstream_id.clone()),
        );
        data.insert("validation_owner".to_owned(), Value::from("merge_steward"));
        if let Some(url) = receipt.context_url.as_deref() {
            data.insert("context_url".to_owned(), Value::from(url));
        }
        write_json_envelope(stdout, "pr.handed-off", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "✓ PR #{pr} exact head {} handed to the durable merge steward; local validation was not duplicated",
            receipt.head.chars().take(12).collect::<String>()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn configured_pr_provenance_hook(
    config: &LoadedConfig,
) -> Result<Option<PrProvenanceHook>, CliFailure> {
    let Some(value) = config.get("pr.provenance.command") else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(CliFailure::new(
            2,
            "pr.provenance.command must be a non-empty TOML string array",
        ));
    };
    let command = items
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| CliFailure::new(2, "pr.provenance.command must contain only strings"))?;
    if command.is_empty() || command[0].is_empty() {
        return Err(CliFailure::new(
            2,
            "pr.provenance.command must be a non-empty TOML string array",
        ));
    }
    let required = config
        .get("pr.provenance.required")
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    Ok(Some(PrProvenanceHook { command, required }))
}

#[allow(clippy::too_many_arguments)]
fn run_pr_provenance_hook<W: Write>(
    config: &LoadedConfig,
    cwd: &Path,
    stdout: &mut W,
    repo: &str,
    branch: &str,
    base: &str,
    head: &str,
    pr: &ResolvedPrContext,
) -> Result<(), CliFailure> {
    let Some(hook) = configured_pr_provenance_hook(config)? else {
        return Ok(());
    };
    let pr_number = pr.number.to_string();
    let pr_url = pr.pr_url.as_deref().unwrap_or_default();
    let values = [
        ("{pr}", pr_number.as_str()),
        ("{repo}", repo),
        ("{head}", head),
        ("{branch}", branch),
        ("{base}", base),
        ("{url}", pr_url),
    ];
    let expand = |argument: &str| {
        values
            .iter()
            .fold(argument.to_owned(), |expanded, (key, value)| {
                expanded.replace(key, value)
            })
    };
    let program = expand(&hook.command[0]);
    let arguments = hook.command[1..]
        .iter()
        .map(|argument| expand(argument))
        .collect::<Vec<_>>();
    let output = Command::new(&program)
        .args(&arguments)
        .current_dir(cwd)
        .env("SHIPYARD_PR_NUMBER", &pr_number)
        .env("SHIPYARD_PR_REPO", repo)
        .env("SHIPYARD_PR_HEAD", head)
        .env("SHIPYARD_PR_BRANCH", branch)
        .env("SHIPYARD_PR_BASE", base)
        .env("SHIPYARD_PR_URL", pr_url)
        .output();
    match output {
        Ok(result) if result.status.success() => {
            writeln!(
                stdout,
                "▸ PR provenance hook completed for #{pr_number} at {}",
                head.chars().take(12).collect::<String>()
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            Ok(())
        }
        Ok(result) => {
            let diagnostic = if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            };
            let detail = String::from_utf8_lossy(diagnostic).trim().to_owned();
            let message = format!(
                "PR provenance hook failed for #{pr_number} at {} (exit {}): {}",
                head.chars().take(12).collect::<String>(),
                result.status.code().unwrap_or(1),
                if detail.is_empty() {
                    "no diagnostic output"
                } else {
                    &detail
                }
            );
            if hook.required {
                Err(CliFailure::new(1, message))
            } else {
                writeln!(stdout, "⚠︎ {message}")
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
                Ok(())
            }
        }
        Err(error) => {
            let message = format!(
                "PR provenance hook failed to start for #{pr_number} at {}: {error}",
                head.chars().take(12).collect::<String>()
            );
            if hook.required {
                Err(CliFailure::new(1, message))
            } else {
                writeln!(stdout, "⚠︎ {message}")
                    .map_err(|write_error| CliFailure::new(1, write_error.to_string()))?;
                Ok(())
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_requested_steward_handoff<W: Write>(
    request: Option<&ShipStewardHandoff>,
    repo: &str,
    head: &str,
    pr: &ResolvedPrContext,
    config: &LoadedConfig,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<Option<AppliedStewardHandoff>, CliFailure> {
    let actions = GitHubActions::from_loaded_config(cwd, config);
    apply_requested_steward_handoff_with_actions(
        request, repo, head, pr, cwd, &actions, json_mode, stdout,
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_requested_steward_handoff_with_actions<W: Write>(
    request: Option<&ShipStewardHandoff>,
    repo: &str,
    head: &str,
    pr: &ResolvedPrContext,
    cwd: &Path,
    actions: &GitHubActions,
    json_mode: bool,
    stdout: &mut W,
) -> Result<Option<AppliedStewardHandoff>, CliFailure> {
    let Some(request) = request else {
        return Ok(None);
    };
    let workstream_id = request
        .workstream_id
        .clone()
        .unwrap_or_else(|| default_steward_workstream(repo, pr.number));
    let context_url = request.context_url.clone().or_else(|| pr.pr_url.clone());
    let mut sink = std::io::sink();
    steward_handoff_command(
        &StewardHandoffArgs {
            repo: Some(repo.to_owned()),
            pr: pr.number,
            head: head.to_owned(),
            workstream_id: workstream_id.clone(),
            context_url: context_url.clone(),
            apply: true,
        },
        cwd,
        actions,
        false,
        &mut sink,
    )?;
    if !json_mode {
        writeln!(
            stdout,
            "▸ Durable steward receipt: PR #{} head={} workstream={workstream_id}",
            pr.number, head
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(Some(AppliedStewardHandoff {
        workstream_id,
        context_url,
        head: head.to_owned(),
    }))
}

fn default_steward_workstream(repo: &str, pr: u64) -> String {
    format!("{}#{pr}", repo.to_ascii_lowercase())
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

pub(super) fn ensure_existing_pr_base_matches(
    actual: &str,
    requested: &str,
) -> Result<(), CliFailure> {
    if actual == requested {
        Ok(())
    } else {
        Err(CliFailure::new(
            2,
            format!(
                "existing PR targets `{actual}`, but this invocation requested `{requested}`; rerun with --base {actual}"
            ),
        ))
    }
}

fn ensure_pr_head_matches(info: &PrInfo, branch: &str, head_sha: &str) -> Result<(), CliFailure> {
    if info.branch == branch && info.head_sha.eq_ignore_ascii_case(head_sha) {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!(
                "PR #{} identity changed after push: expected branch `{branch}` at `{head_sha}`, found `{}` at `{}`",
                info.number, info.branch, info.head_sha
            ),
        ))
    }
}

fn resolve_pr_context(
    config: &LoadedConfig,
    args: &ShipCommandArgs,
    cwd: &Path,
    branch: &str,
    lane_policy: &LanePolicy,
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
            return Ok(ResolvedPrContext {
                number,
                base_branch,
                pr_url: None,
                pr_title: None,
            });
        }
        let info = get_pr_status(config, cwd, args.gh_command.as_deref(), &number.to_string())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ResolvedPrContext {
            number,
            base_branch: info.base,
            pr_url: Some(info.url),
            pr_title: Some(info.title),
        });
    }

    let existing = find_pr_for_branch(config, cwd, args.gh_command.as_deref(), branch)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if let Some(info) = existing.as_ref() {
        ensure_existing_pr_base_matches(&info.base, &args.base)?;
    }

    push_branch(cwd, branch).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let expected_head = git_required(cwd, &["rev-parse", "HEAD"])?;
    let info = if let Some(info) = existing {
        get_pr_status(
            config,
            cwd,
            args.gh_command.as_deref(),
            &info.number.to_string(),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?
    } else {
        find_pr_for_branch(config, cwd, args.gh_command.as_deref(), branch)
            .map_err(|error| CliFailure::new(1, error.to_string()))?
            .map_or_else(
                || {
                    create_current_branch_pr(
                        config,
                        cwd,
                        args.gh_command.as_deref(),
                        branch,
                        &args.base,
                        lane_policy,
                    )
                },
                Ok::<PrInfo, CliFailure>,
            )?
    };
    ensure_existing_pr_base_matches(&info.base, &args.base)?;
    ensure_pr_head_matches(&info, branch, &expected_head)?;
    Ok(ResolvedPrContext {
        number: info.number,
        base_branch: info.base,
        pr_url: Some(info.url),
        pr_title: Some(info.title),
    })
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
    /// A [`GreenNotMerged`](Self::GreenNotMerged) whose block is classified as a
    /// *flaky required leg*: the merge was rejected because a required check is
    /// RED on the exact SHA Shipyard validated green, and every red required
    /// check maps to a Shipyard-validated-green target. Renders the one-liner
    /// `shipyard rescue` recovery instead of the generic hand-back. Still
    /// `merged() == false` — this only changes the guidance, not the outcome.
    GreenNotMergedFlakyRequired {
        /// The underlying `gh pr merge` error, surfaced verbatim.
        error: String,
        /// Names of the red required checks (all validated green by Shipyard).
        red_contexts: Vec<String>,
    },
    /// Validation passed and nothing about the PR blocked the merge — Shipyard
    /// sent GitHub a malformed GraphQL query. A *client* defect: the operator
    /// cannot fix it by waiting or by editing branch protection, and the PR is
    /// very likely mergeable right now. Rendered and exit-coded separately so a
    /// green PR stalled by a Shipyard bug never reads as a red one.
    GreenNotMergedClientDefect(String),
    /// The live PR head advanced past the SHA Shipyard validated (issue #321),
    /// so the merge was refused rather than landing unvalidated work. Branch
    /// protection is not involved and waiting cannot clear it — the fix is
    /// always to re-ship the new head — so this carries its own render instead
    /// of the generic branch-protection guidance.
    GreenNotMergedHeadSuperseded {
        /// The SHA Shipyard validated green.
        validated: String,
        /// The SHA that is live on the PR now.
        current: String,
    },
}

impl ShipRenderState {
    fn merged(&self) -> bool {
        matches!(self, Self::Merged)
    }

    /// The merge-rejection reason for states that carry one. Verbatim where a
    /// `gh` error exists; composed for the superseded-head case, which Shipyard
    /// refuses client-side without ever calling `gh`.
    fn merge_error(&self) -> Option<Cow<'_, str>> {
        match self {
            Self::ValidationFailed | Self::Merged => None,
            Self::GreenNotMerged(error)
            | Self::GreenNotMergedClientDefect(error)
            | Self::GreenNotMergedFlakyRequired { error, .. } => Some(Cow::Borrowed(error)),
            Self::GreenNotMergedHeadSuperseded { validated, current } => Some(Cow::Owned(format!(
                "live PR head {current} superseded the validated SHA {validated}; re-run shipyard ship to validate the new head"
            ))),
        }
    }

    /// Stable machine-readable tag for the `--json` envelope, so automation can
    /// tell "validation failed" from "validated green, merge call malformed"
    /// without pattern-matching on prose.
    fn status(&self) -> &'static str {
        match self {
            Self::ValidationFailed => "validation_failed",
            Self::Merged => "merged",
            Self::GreenNotMerged(_) => "green_not_merged",
            Self::GreenNotMergedFlakyRequired { .. } => "green_not_merged_flaky_required",
            Self::GreenNotMergedClientDefect(_) => "green_not_merged_client_defect",
            Self::GreenNotMergedHeadSuperseded { .. } => "green_not_merged_head_superseded",
        }
    }

    /// Process exit code. `validation_failed` keeps its historical `1`, and the
    /// green-but-blocked states keep their historical `0` so existing callers do
    /// not change meaning — only the new client-defect state gets a distinct,
    /// nonzero code, because that one is a Shipyard bug an operator must see.
    fn exit_code(&self) -> ExitCode {
        match self {
            Self::ValidationFailed => ExitCode::from(1),
            Self::GreenNotMergedClientDefect(_) => ExitCode::from(SHIP_EXIT_MERGE_CLIENT_DEFECT),
            Self::Merged
            | Self::GreenNotMerged(_)
            | Self::GreenNotMergedFlakyRequired { .. }
            | Self::GreenNotMergedHeadSuperseded { .. } => ExitCode::SUCCESS,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn post_run_merge_state(
    pr: u64,
    cwd: &Path,
    store: &ShipStateStore,
    config: &LoadedConfig,
    mode: RuntimeMode,
    repo: &str,
    validation_passed: bool,
    merge_command: Option<PathBuf>,
    merge_result: Option<MergeResult>,
    pr_snapshot_file: Option<PathBuf>,
) -> Result<ShipRenderState, CliFailure> {
    if !validation_passed {
        return Ok(ShipRenderState::ValidationFailed);
    }
    let request = AutoMergeRequest {
        mode,
        global_dir: config.global_dir.clone(),
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
        AutoMergeOutcome::Enqueued => {
            match supervise_merge_queue(store, cwd, mode, &config.global_dir, pr, true) {
                AutoMergeOutcome::Merged { .. } | AutoMergeOutcome::AlreadyMerged => {
                    Ok(ShipRenderState::Merged)
                }
                AutoMergeOutcome::SupersededSha { validated, current } => {
                    Ok(ShipRenderState::GreenNotMergedHeadSuperseded { validated, current })
                }
                // Queue supervision re-runs the merge-queue poll query, so it can
                // surface the same malformed-query defect as admission does.
                AutoMergeOutcome::MergeFailed { error } => Ok(green_not_merged(error)),
                AutoMergeOutcome::Enqueued
                | AutoMergeOutcome::PrNotFound
                | AutoMergeOutcome::InFlight { .. }
                | AutoMergeOutcome::TargetFailed { .. } => Err(CliFailure::new(
                    1,
                    format!("PR #{pr}: merge-queue supervision ended without a terminal verdict"),
                )),
            }
        }
        AutoMergeOutcome::MergeFailed { error } => {
            Ok(classify_merge_failure(store, config, cwd, repo, pr, error))
        }
        // Validation passed but the live head advanced past the validated SHA
        // (issue #321): the green evidence describes a commit that is no longer
        // the head, so merging it would land unvalidated work.
        AutoMergeOutcome::SupersededSha { validated, current } => {
            Ok(ShipRenderState::GreenNotMergedHeadSuperseded { validated, current })
        }
        AutoMergeOutcome::PrNotFound
        | AutoMergeOutcome::InFlight { .. }
        | AutoMergeOutcome::TargetFailed { .. } => Err(CliFailure::new(
            1,
            format!("PR #{pr}: validation passed but ship-state was not merge-ready"),
        )),
    }
}

/// Green-but-unmerged hand-back for paths with no rollup context to inspect.
/// Still splits out the client-defect case, because "Shipyard sent a malformed
/// query" must never render as "the PR is blocked".
fn green_not_merged(error: String) -> ShipRenderState {
    if is_graphql_malformed_query_error(&error) {
        ShipRenderState::GreenNotMergedClientDefect(error)
    } else {
        ShipRenderState::GreenNotMerged(error)
    }
}

/// Classify a validated-green-but-`gh pr merge`-rejected wedge. Returns
/// [`ShipRenderState::GreenNotMergedFlakyRequired`] only when the block is a
/// flaky required leg (a required check RED on the exact SHA Shipyard validated
/// green, every red required check mapping to a validated-green target) so the
/// hand-back can point at the one-liner recovery. Fails closed to
/// [`ShipRenderState::GreenNotMerged`] on any ambiguity — state unreadable,
/// rollup fetch failed, or the wedge is not a recognised flake. This never
/// mutates the merge path; it only picks which guidance to render.
fn classify_merge_failure(
    store: &ShipStateStore,
    config: &LoadedConfig,
    cwd: &Path,
    repo: &str,
    pr: u64,
    error: String,
) -> ShipRenderState {
    // Check the client-defect case before anything that inspects the PR: a
    // malformed GraphQL query says nothing about the PR's mergeability, and the
    // rollup-based flaky-leg classification below would only add noise.
    if is_graphql_malformed_query_error(&error) {
        return ShipRenderState::GreenNotMergedClientDefect(error);
    }
    let Some(state) = store.get(pr) else {
        return ShipRenderState::GreenNotMerged(error);
    };
    let green = validated_green_contexts(&state, config);
    if green.is_empty() {
        return ShipRenderState::GreenNotMerged(error);
    }
    // Fail closed: without a trustworthy rollup we cannot prove the block is a
    // flaky required leg, so fall back to the generic hand-back.
    let Ok((live_head, rollup)) =
        fetch_head_and_status_check_rollup_with_cwd(RuntimeMode::Shipyard, cwd, repo, pr)
    else {
        return ShipRenderState::GreenNotMerged(error);
    };
    // Prove the rollup describes the exact SHA Shipyard validated. If the head
    // advanced between the failed merge and this fetch, the rollup can describe
    // an unvalidated SHA — never claim "the SHA Shipyard validated green" then.
    if !sha_matches(&live_head, &state.head_sha) {
        return ShipRenderState::GreenNotMerged(error);
    }
    match classify_wedge(&WedgeInputs {
        rollup: &rollup,
        validated_green_contexts: &green,
    }) {
        WedgeClass::FlakyRequired { red_contexts } => {
            ShipRenderState::GreenNotMergedFlakyRequired {
                error,
                red_contexts,
            }
        }
        WedgeClass::RequiredStillPending | WedgeClass::NotRecoverable { .. } => {
            ShipRenderState::GreenNotMerged(error)
        }
    }
}

fn render_json<W: Write>(
    stdout: &mut W,
    pr: u64,
    outcome: &crate::ship::ShipExecutionOutcome,
    state: &ShipRenderState,
    diagnostics: &[RenderedDiagnostics],
    steward_handoff: Option<&AppliedStewardHandoff>,
) -> Result<(), CliFailure> {
    let merged = state.merged();
    // Only the flaky-required wedge carries recovery contexts; every other
    // state leaves this an empty array so the envelope shape stays stable.
    let flaky_recovery: Vec<Value> = match state {
        ShipRenderState::GreenNotMergedFlakyRequired { red_contexts, .. } => red_contexts
            .iter()
            .map(|name| Value::String(name.clone()))
            .collect(),
        _ => Vec::new(),
    };
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
            // `merged:false` alone cannot tell a caller whether validation failed
            // or validation passed and only the merge call broke. These two do.
            ("status", Value::from(state.status())),
            (
                "merge_error",
                state.merge_error().map_or(Value::Null, Value::from),
            ),
            ("run", outcome.job.to_json_value()),
            ("ship_state", json!(outcome.ship_state)),
            (
                "resumed_existing_state",
                Value::Bool(outcome.resumed_existing_state),
            ),
            ("diagnostics", Value::Array(diag_payload)),
            ("flaky_required_recovery", Value::Array(flaky_recovery)),
            (
                "steward_handoff",
                steward_handoff.map_or(Value::Null, |receipt| {
                    json!({
                        "context": "shipyard/steward-handoff",
                        "state": "success",
                        "head": receipt.head,
                        "workstream_id": receipt.workstream_id,
                        "context_url": receipt.context_url,
                    })
                }),
            ),
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
        ShipRenderState::GreenNotMergedClientDefect(error) => {
            render_green_not_merged_client_defect(stdout, pr, error)
        }
        ShipRenderState::GreenNotMergedHeadSuperseded { validated, current } => {
            render_green_not_merged_head_superseded(stdout, pr, validated, current)
        }
        ShipRenderState::GreenNotMergedFlakyRequired {
            error,
            red_contexts,
        } => render_green_not_merged_flaky(stdout, pr, error, red_contexts),
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

/// Hand-back for a merge blocked by Shipyard's *own* malformed request. The
/// generic [`render_green_not_merged`] guidance is actively wrong here: it blames
/// branch protection and unsupervised checks, sending the reader to investigate a
/// PR that is very likely mergeable. Name the defect, and give the unblock that
/// works — the merge itself is safe to arm, because every gate already passed.
fn render_green_not_merged_client_defect<W: Write>(
    stdout: &mut W,
    pr: u64,
    error: &str,
) -> std::io::Result<()> {
    writeln!(
        stdout,
        "Shipyard-validated targets passed. The merge was NOT rejected by GitHub's"
    )?;
    writeln!(
        stdout,
        "branch protection — Shipyard sent a malformed GraphQL request:"
    )?;
    writeln!(stdout, "  reason: {error}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "This is a Shipyard defect, not a problem with PR #{pr}. Waiting will not"
    )?;
    writeln!(
        stdout,
        "clear it and branch protection is not worth investigating. Please report it"
    )?;
    writeln!(
        stdout,
        "with the reason above: https://github.com/danielraffel/Shipyard/issues"
    )?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "To land PR #{pr} now — every gate already passed, so this bypasses nothing:"
    )?;
    writeln!(stdout, "  gh pr merge {pr} --auto")?;
    writeln!(
        stdout,
        "Omit any merge-strategy flag: on a merge-queue-governed branch the queue"
    )?;
    writeln!(
        stdout,
        "owns the strategy and `--squash` is refused. Add it only if the base branch"
    )?;
    writeln!(stdout, "has no merge queue.")?;
    Ok(())
}

/// Hand-back for a merge Shipyard itself refused because the head moved. GitHub
/// never rejected anything here, so the generic branch-protection guidance is
/// wrong twice over: there is no protection rule to inspect, and waiting cannot
/// help — the green evidence describes a commit that is no longer the head. The
/// only fix is to validate the new head.
fn render_green_not_merged_head_superseded<W: Write>(
    stdout: &mut W,
    pr: u64,
    validated: &str,
    current: &str,
) -> std::io::Result<()> {
    writeln!(
        stdout,
        "Shipyard-validated targets passed, but PR #{pr} was NOT merged: its head"
    )?;
    writeln!(stdout, "moved after validation completed.")?;
    writeln!(stdout, "  validated: {validated}")?;
    writeln!(stdout, "  live head: {current}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "Merging now would land a commit no target ever validated, so Shipyard"
    )?;
    writeln!(
        stdout,
        "refused. GitHub rejected nothing — branch protection is not involved and"
    )?;
    writeln!(stdout, "waiting will not clear it. Validate the new head:")?;
    writeln!(stdout, "  shipyard ship --pr {pr} --adopt-head")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "If you did not expect the head to move, check for an unpushed local commit"
    )?;
    writeln!(stdout, "or a concurrent push before re-shipping.")?;
    Ok(())
}

/// Recovery guidance for a *flaky required leg* wedge — a required check that is
/// RED on the exact SHA Shipyard validated green. Unlike the generic hand-back,
/// this is a known-recoverable case: re-dispatch the flaky leg and arm the
/// merge, both one-liners. Motivated by the ~hour lost hand-cranking
/// cancel+rerun when the `macos` required leg flaked under runner load.
fn render_green_not_merged_flaky<W: Write>(
    stdout: &mut W,
    pr: u64,
    error: &str,
    red_contexts: &[String],
) -> std::io::Result<()> {
    let checks = red_contexts.join(", ");
    writeln!(
        stdout,
        "Shipyard-validated targets passed, but the merge was rejected for PR #{pr}:"
    )?;
    writeln!(stdout, "  reason: {error}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "Required check(s) [{checks}] are RED on the exact SHA Shipyard just"
    )?;
    writeln!(
        stdout,
        "validated green — a flaky required leg, not a real regression. Recover it:"
    )?;
    writeln!(
        stdout,
        "  * re-dispatch the flaky leg:   `shipyard rescue {pr} --rerun-failed`"
    )?;
    writeln!(
        stdout,
        "  * arm the merge for when it's green: `gh pr merge {pr} --squash --auto`"
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

    #[cfg(unix)]
    use super::run_pr_provenance_hook;
    use super::{
        SHIP_EXIT_MERGE_CLIENT_DEFECT, ShipCommandArgs, ShipInvocation, ShipRenderState,
        ShipStewardHandoff, configured_pr_provenance_hook, ensure_existing_pr_base_matches,
        git_required, green_not_merged, is_terminal_steward_handoff, render_green_not_merged,
        render_green_not_merged_client_defect, render_green_not_merged_flaky,
        render_green_not_merged_head_superseded, resolve_pr_context, ship_command,
    };
    use crate::app::cli::MergeResult;
    #[cfg(unix)]
    use crate::cloud::GitHubActions;
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use crate::identity::RuntimeMode;
    use crate::paths::RuntimePaths;

    #[test]
    fn existing_pr_base_must_match_the_requested_policy_base() {
        assert!(ensure_existing_pr_base_matches("main", "main").is_ok());
        let error = ensure_existing_pr_base_matches("release", "main")
            .expect_err("mismatched base must fail closed");
        assert!(error.message().contains("existing PR targets `release`"));
        assert!(error.message().contains("rerun with --base release"));
    }

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
        assert!(!ShipRenderState::GreenNotMergedClientDefect("err".to_owned()).merged());
        assert!(
            !ShipRenderState::GreenNotMergedFlakyRequired {
                error: "err".to_owned(),
                red_contexts: vec!["macos".to_owned()],
            }
            .merged()
        );
    }

    /// The exact stderr from the `autoMergeRequest{id}` schema bug must land in
    /// the client-defect state, not in the generic branch-protection hand-back.
    #[test]
    fn malformed_graphql_query_classifies_as_a_client_defect() {
        let err = "gh: Field 'id' doesn't exist on type 'AutoMergeRequest'".to_owned();
        assert_eq!(
            green_not_merged(err.clone()),
            ShipRenderState::GreenNotMergedClientDefect(err)
        );
    }

    #[test]
    fn genuine_merge_rejection_stays_a_generic_green_not_merged() {
        let err = "gh: Required status check \"macos\" is expected.".to_owned();
        assert_eq!(
            green_not_merged(err.clone()),
            ShipRenderState::GreenNotMerged(err)
        );
    }

    /// A green PR stalled by a Shipyard defect must be distinguishable from a red
    /// one by exit code alone, while every pre-existing state keeps its code.
    #[test]
    fn client_defect_gets_a_distinct_nonzero_exit_code() {
        assert_eq!(
            format!("{:?}", ShipRenderState::Merged.exit_code()),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert_eq!(
            format!("{:?}", ShipRenderState::ValidationFailed.exit_code()),
            format!("{:?}", ExitCode::from(1))
        );
        assert_eq!(
            format!(
                "{:?}",
                ShipRenderState::GreenNotMerged("e".to_owned()).exit_code()
            ),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert_eq!(
            format!(
                "{:?}",
                ShipRenderState::GreenNotMergedClientDefect("e".to_owned()).exit_code()
            ),
            format!("{:?}", ExitCode::from(SHIP_EXIT_MERGE_CLIENT_DEFECT))
        );
        // Must not collide with validation-failed, or the distinction is lost.
        assert_ne!(SHIP_EXIT_MERGE_CLIENT_DEFECT, 0);
        assert_ne!(SHIP_EXIT_MERGE_CLIENT_DEFECT, 1);
    }

    #[test]
    fn json_status_and_merge_error_separate_the_failure_modes() {
        assert_eq!(ShipRenderState::Merged.status(), "merged");
        assert_eq!(ShipRenderState::Merged.merge_error(), None);
        assert_eq!(
            ShipRenderState::ValidationFailed.status(),
            "validation_failed"
        );
        assert_eq!(ShipRenderState::ValidationFailed.merge_error(), None);

        let err = "gh: Field 'id' doesn't exist on type 'AutoMergeRequest'";
        let defect = ShipRenderState::GreenNotMergedClientDefect(err.to_owned());
        assert_eq!(defect.status(), "green_not_merged_client_defect");
        assert_eq!(defect.merge_error().as_deref(), Some(err));

        let blocked = ShipRenderState::GreenNotMerged("blocked".to_owned());
        assert_eq!(blocked.status(), "green_not_merged");
        assert_eq!(blocked.merge_error().as_deref(), Some("blocked"));

        // Shipyard refuses this one client-side, so there is no `gh` error to
        // quote — the envelope still has to carry a reason.
        let superseded = ShipRenderState::GreenNotMergedHeadSuperseded {
            validated: "aaaa".to_owned(),
            current: "bbbb".to_owned(),
        };
        assert_eq!(superseded.status(), "green_not_merged_head_superseded");
        let reason = superseded.merge_error().expect("reason");
        assert!(reason.contains("aaaa"), "must name the validated SHA");
        assert!(reason.contains("bbbb"), "must name the live SHA");

        // No two green-but-unmerged states may share a status tag.
        let tags = [blocked.status(), defect.status(), superseded.status()];
        assert_eq!(
            tags.len(),
            tags.iter().collect::<std::collections::BTreeSet<_>>().len(),
            "status tags must be distinct: {tags:?}"
        );
    }

    /// Shipyard refuses a superseded head itself — GitHub rejected nothing — so
    /// the render must not send the reader to branch protection.
    #[test]
    fn head_superseded_render_does_not_blame_branch_protection() {
        let mut buf = Vec::<u8>::new();
        render_green_not_merged_head_superseded(&mut buf, 384, "aaaa111", "bbbb222")
            .expect("render");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(
            out.contains("aaaa111"),
            "must name validated SHA; got:\n{out}"
        );
        assert!(out.contains("bbbb222"), "must name live SHA; got:\n{out}");
        assert!(
            !out.contains("branch protection requires"),
            "must NOT blame branch protection; got:\n{out}"
        );
        assert!(
            out.contains("--adopt-head"),
            "must point at the re-ship that adopts the new head; got:\n{out}"
        );
    }

    /// The generic render blames branch protection, which is wrong for this
    /// failure. The client-defect render must not repeat that misdirection.
    #[test]
    fn client_defect_render_blames_shipyard_not_branch_protection() {
        let mut buf = Vec::<u8>::new();
        let err = "gh: Field 'id' doesn't exist on type 'AutoMergeRequest'";
        render_green_not_merged_client_defect(&mut buf, 6682, err).expect("render");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(out.contains("PR #6682"), "must name the PR; got:\n{out}");
        assert!(
            out.contains(err),
            "must surface the error verbatim; got:\n{out}"
        );
        assert!(
            out.contains("malformed GraphQL request"),
            "must name the actual cause; got:\n{out}"
        );
        assert!(
            out.contains("Shipyard defect"),
            "must attribute the fault to Shipyard; got:\n{out}"
        );
        assert!(
            !out.contains("branch protection requires"),
            "must NOT repeat the branch-protection misdirection; got:\n{out}"
        );
        // The queue owns the strategy on a queue-governed branch, so the
        // suggested unblock must not hardcode --squash.
        assert!(
            out.contains("gh pr merge 6682 --auto"),
            "must offer a strategy-free unblock; got:\n{out}"
        );
        assert!(
            !out.contains("--squash --auto"),
            "must not suggest a strategy the merge queue would refuse; got:\n{out}"
        );
    }

    #[test]
    fn flaky_required_render_points_at_the_rescue_one_liner() {
        let mut out = Vec::new();
        render_green_not_merged_flaky(
            &mut out,
            2020,
            "base branch policy prohibits the merge",
            &["macos".to_owned()],
        )
        .expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("shipyard rescue 2020 --rerun-failed"),
            "must hand the operator the one-liner rescue; got:\n{text}"
        );
        assert!(
            text.contains("macos"),
            "must name the flaky required check; got:\n{text}"
        );
        assert!(
            text.contains("flaky required leg"),
            "must explain the block is a flake, not a regression; got:\n{text}"
        );
        assert!(
            !text.contains("All green"),
            "must not claim all green; got:\n{text}"
        );
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

    #[test]
    fn provenance_hook_config_is_argv_only_and_required_by_default() {
        let mut config = loaded_config(std::path::Path::new("."));
        let extra = r#"
            [pr.provenance]
            command = ["whence", "--pr", "{pr}", "--auto"]
        "#
        .parse::<Table>()
        .expect("hook TOML");
        config.data.extend(extra);
        let hook = configured_pr_provenance_hook(&config)
            .expect("valid config")
            .expect("configured hook");
        assert_eq!(hook.command, ["whence", "--pr", "{pr}", "--auto"]);
        assert!(hook.required);

        config.data.insert(
            "pr".to_owned(),
            toml::Value::Table(
                r#"[provenance]
                   command = "whence --auto""#
                    .parse::<Table>()
                    .expect("invalid-shape fixture"),
            ),
        );
        let error = configured_pr_provenance_hook(&config).expect_err("string must fail");
        assert!(error.message().contains("TOML string array"));
    }

    #[cfg(unix)]
    #[test]
    fn provenance_hook_gets_exact_pr_facts_before_handoff() {
        let temp = tempfile::tempdir().expect("tempdir");
        let hook = temp.path().join("provenance-hook");
        let log = temp.path().join("hook.log");
        fake_gh(
            &hook,
            r#"log=$1
shift
printf '%s\n' "$SHIPYARD_PR_NUMBER|$SHIPYARD_PR_REPO|$SHIPYARD_PR_HEAD|$SHIPYARD_PR_BRANCH|$SHIPYARD_PR_BASE|$SHIPYARD_PR_URL|$*" > "$log""#,
        );
        let mut config = loaded_config(temp.path());
        let extra = format!(
            r#"[pr.provenance]
command = [{hook:?}, {log:?}, "{{pr}}", "{{repo}}", "{{head}}", "{{branch}}", "{{base}}", "{{url}}"]
required = true
"#,
            hook = hook.display().to_string(),
            log = log.display().to_string(),
        )
        .parse::<Table>()
        .expect("hook config");
        config.data.extend(extra);
        let pr = super::ResolvedPrContext {
            number: 42,
            base_branch: "main".to_owned(),
            pr_url: Some("https://github.com/danielraffel/pulp/pull/42".to_owned()),
            pr_title: Some("Fix".to_owned()),
        };
        let head = "a".repeat(40);
        let mut stdout = Vec::new();
        run_pr_provenance_hook(
            &config,
            temp.path(),
            &mut stdout,
            "danielraffel/pulp",
            "feature/provenance",
            "main",
            &head,
            &pr,
        )
        .expect("hook succeeds");
        let recorded = std::fs::read_to_string(log).expect("hook log");
        assert_eq!(
            recorded.trim(),
            format!(
                "42|danielraffel/pulp|{head}|feature/provenance|main|https://github.com/danielraffel/pulp/pull/42|42 danielraffel/pulp {head} feature/provenance main https://github.com/danielraffel/pulp/pull/42"
            )
        );
        assert!(
            String::from_utf8(stdout)
                .expect("utf8")
                .contains("completed for #42")
        );
    }

    #[cfg(unix)]
    #[test]
    fn required_provenance_hook_fails_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let hook = temp.path().join("provenance-hook");
        fake_gh(&hook, "printf 'missing provenance' >&2\nexit 7");
        let mut config = loaded_config(temp.path());
        let extra = format!(
            r"[pr.provenance]
command = [{hook:?}]
",
            hook = hook.display().to_string(),
        )
        .parse::<Table>()
        .expect("hook config");
        config.data.extend(extra);
        let pr = super::ResolvedPrContext {
            number: 42,
            base_branch: "main".to_owned(),
            pr_url: None,
            pr_title: None,
        };
        let error = run_pr_provenance_hook(
            &config,
            temp.path(),
            &mut Vec::new(),
            "danielraffel/pulp",
            "feature/provenance",
            "main",
            &"a".repeat(40),
            &pr,
        )
        .expect_err("required hook must fail");
        assert_eq!(error.code, 1);
        assert!(error.message().contains("exit 7"));
        assert!(error.message().contains("missing provenance"));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_handoff_uses_pr_fallback_and_writes_status_before_label() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = temp.path().join("gh");
        let log = temp.path().join("gh.log");
        let managed = temp.path().join("managed");
        let head = "a".repeat(40);
        fake_gh(
            &gh,
            &format!(
                r#"printf '%s\n' "$*" >> '{}'
case "$*" in
  *"repos/danielraffel/pulp/pulls/42"*)
    if [ -f '{managed}' ]; then labels='[{{"name":"shipyard:managed"}}]'; else labels='[]'; fi
    printf '%s\n' "{{\"state\":\"open\",\"head\":{{\"sha\":\"{head}\"}},\"labels\":$labels}}"
    ;;
  *"repos/danielraffel/pulp/issues/42/labels"*)
    : > '{managed}'
    printf '%s\n' '{{}}'
    ;;
  *"repos/danielraffel/pulp/commits/{head}/status"*)
    printf '%s\n' '{{"statuses":[{{"context":"shipyard/steward-handoff","state":"success","description":"Managed handoff danielraffel/pulp#42","target_url":"https://github.com/danielraffel/pulp/pull/42"}}]}}'
    ;;
  *) printf '%s\n' '{{}}' ;;
esac"#,
                log.display(),
                managed = managed.display()
            ),
        );
        let config = loaded_config(temp.path());
        let actions =
            GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(&gh);
        let request = super::ShipStewardHandoff {
            workstream_id: None,
            context_url: None,
        };
        let pr = super::ResolvedPrContext {
            number: 42,
            base_branch: String::from("main"),
            pr_url: Some(String::from("https://github.com/danielraffel/pulp/pull/42")),
            pr_title: Some(String::from("Fix")),
        };

        let receipt = super::apply_requested_steward_handoff_with_actions(
            Some(&request),
            "danielraffel/pulp",
            &head,
            &pr,
            temp.path(),
            &actions,
            true,
            &mut Vec::new(),
        )
        .expect("handoff")
        .expect("receipt");

        assert_eq!(receipt.workstream_id, "danielraffel/pulp#42");
        assert_eq!(
            receipt.context_url.as_deref(),
            Some(pr.pr_url.as_deref().unwrap())
        );
        let calls = std::fs::read_to_string(log).expect("gh log");
        let status = calls.find("statuses/").expect("status call");
        let label = calls.find("issues/42/labels").expect("label call");
        assert!(status < label, "status receipt must precede managed label");
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
    fn only_pr_invocations_with_a_steward_request_skip_local_validation() {
        let args = |invocation, steward_handoff| ShipCommandArgs {
            pr: None,
            base: "main".to_owned(),
            auto_create_base: None,
            no_warm: false,
            resume_from: None,
            merge_command: None,
            merge_result: None,
            gh_command: None,
            pr_snapshot_file: None,
            allow_unreachable_targets: false,
            allow_fleet_epoch_drift: false,
            skip_targets: Vec::new(),
            adopt_head: false,
            steward_handoff,
            invocation,
        };
        assert!(is_terminal_steward_handoff(&args(
            ShipInvocation::PrCommand,
            Some(ShipStewardHandoff {
                workstream_id: None,
                context_url: None,
            })
        )));
        assert!(!is_terminal_steward_handoff(&args(
            ShipInvocation::Direct,
            Some(ShipStewardHandoff {
                workstream_id: None,
                context_url: None,
            })
        )));
        assert!(!is_terminal_steward_handoff(&args(
            ShipInvocation::PrCommand,
            None
        )));
    }

    #[test]
    fn default_steward_workstream_uses_the_canonical_repository_slug() {
        assert_eq!(
            super::default_steward_workstream("Generous-Corp/Forge", 7),
            "generous-corp/forge#7"
        );
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
        std::fs::write(&snapshot, format!(r#"{{"headRefOid":"{head}"}}"#)).expect("write snapshot");
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
                allow_fleet_epoch_drift: false,
                skip_targets: Vec::new(),
                adopt_head: false,
                steward_handoff: None,
                invocation: ShipInvocation::Direct,
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
                allow_fleet_epoch_drift: false,
                skip_targets: Vec::new(),
                adopt_head: false,
                steward_handoff: None,
                invocation: ShipInvocation::Direct,
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
                allow_fleet_epoch_drift: false,
                skip_targets: Vec::new(),
                adopt_head: false,
                steward_handoff: None,
                invocation: ShipInvocation::Direct,
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
        let head = git_capture(&["rev-parse", "HEAD"], &repo);
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            format!(r#"{{"headRefOid":"{head}","baseRefName":"main"}}"#),
        )
        .expect("write snapshot");
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
                pr_snapshot_file: Some(snapshot),
                allow_unreachable_targets: false,
                allow_fleet_epoch_drift: false,
                skip_targets: vec!["linux".to_owned()],
                adopt_head: false,
                steward_handoff: None,
                invocation: ShipInvocation::Direct,
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
        let expected_head = git_required(&repo, &["rev-parse", "HEAD"]).expect("head");
        let gh = temp.path().join("gh");
        let gh_log = temp.path().join("gh.log");
        fake_gh(
            &gh,
            &format!(
                r#"
echo "$@" >> "{}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":88,"url":"https://github.com/o/r/pull/88","title":"Existing PR","state":"OPEN","headRefName":"feature/test","headRefOid":"{1}","baseRefName":"main"}}]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"number":88,"url":"https://github.com/o/r/pull/88","title":"Existing PR","state":"OPEN","headRefName":"feature/test","headRefOid":"{1}","baseRefName":"main"}}'
  exit 0
fi
echo "unexpected gh args: $@" >&2
exit 2
"#,
                gh_log.display(),
                expected_head
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
                allow_fleet_epoch_drift: false,
                skip_targets: Vec::new(),
                adopt_head: false,
                steward_handoff: None,
                invocation: ShipInvocation::Direct,
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
    fn mismatched_existing_pr_base_is_rejected_before_push() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        seed_repo_with_local_origin(&repo, &remote);
        std::fs::write(repo.join("feature.txt"), "local change\n").expect("feature");
        git(&["add", "."], &repo);
        git(&["commit", "-q", "-m", "Local head"], &repo);

        let gh = temp.path().join("gh");
        fake_gh(
            &gh,
            r#"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{"number":88,"url":"https://github.com/o/r/pull/88","title":"Wrong base","state":"OPEN","headRefName":"feature/test","headRefOid":"1111111111111111111111111111111111111111","baseRefName":"release"}]'
  exit 0
fi
echo "unexpected gh args: $@" >&2
exit 2
"#,
        );
        let config = loaded_config(temp.path());
        let args = ShipCommandArgs {
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
            allow_fleet_epoch_drift: false,
            skip_targets: Vec::new(),
            adopt_head: false,
            steward_handoff: None,
            invocation: ShipInvocation::Direct,
        };
        let lane_policy = crate::lane_policy::resolve_lane_policy(&config, &repo);

        let Err(error) = resolve_pr_context(&config, &args, &repo, "feature/test", &lane_policy)
        else {
            panic!("wrong base must fail before push");
        };
        assert!(error.message().contains("existing PR targets `release`"));
        let remote_ref = crate::supervised::git_supervised()
            .args(["ls-remote", "origin", "refs/heads/feature/test"])
            .current_dir(&repo)
            .output()
            .expect("ls-remote");
        assert!(remote_ref.status.success());
        assert!(remote_ref.stdout.is_empty(), "branch must not be pushed");
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
        let expected_head = git_required(&repo, &["rev-parse", "HEAD"]).expect("head");
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
  echo '{{"number":89,"url":"https://github.com/o/r/pull/89","title":"Add autopilot","state":"OPEN","headRefName":"feature/test","headRefOid":"{1}","baseRefName":"develop/test"}}'
  exit 0
fi
echo "unexpected gh args: $@" >&2
exit 2
"#,
                gh_log.display(),
                expected_head
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
                allow_fleet_epoch_drift: false,
                skip_targets: Vec::new(),
                adopt_head: false,
                steward_handoff: None,
                invocation: ShipInvocation::Direct,
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
