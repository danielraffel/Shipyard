use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    CliFailure,
    cli::{CloudAddLaneArgs, CloudCommand, CloudHandoffCommand, CloudRetargetArgs, CloudRunArgs},
    cloud_read_cmd::{cloud_defaults, cloud_status, cloud_workflows},
    wait_cmd::parse_github_repo_slug,
};
use crate::cloud::{
    CloudDispatchOverrides, CloudDispatchPlan, GitHubActions, GitHubError, QueuedRun,
    WorkflowDefinition, WorkflowJob, default_workflow_key, discover_workflows, lane_is_required,
    resolve_cloud_dispatch_plan, resolve_cloud_dispatch_plan_with_overrides,
};
use crate::cloud_records::{CloudRecordStore, CloudRunRecord};
use crate::config::LoadedConfig;
use crate::output::write_json_envelope;
use crate::ship_state::{DispatchedRun, ShipState, ShipStateStore};
use crate::watch::ship_terminal_verdict;

const DISPATCH_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const DISPATCH_DISCOVERY_POLL: Duration = Duration::from_secs(5);

pub(super) fn cloud_command<W: Write>(
    command: CloudCommand,
    store: &ShipStateStore,
    cloud_records: &CloudRecordStore,
    config: &LoadedConfig,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let actions = GitHubActions::from_loaded_config(cwd, config);
    match command {
        CloudCommand::AddLane(args) => add_lane(&args, store, config, cwd, &actions, json, stdout),
        CloudCommand::Defaults => cloud_defaults(config, cwd, json, stdout),
        CloudCommand::Handoff { command } => handoff(command, config, cwd, &actions, json, stdout),
        CloudCommand::Retarget(args) => retarget(&args, store, config, cwd, &actions, json, stdout),
        CloudCommand::Run(args) => {
            cloud_run(&args, cloud_records, config, cwd, &actions, json, stdout)
        }
        CloudCommand::Status {
            identifier,
            limit,
            refresh,
            no_refresh,
        } => cloud_status(
            cloud_records,
            &actions,
            identifier.as_deref(),
            limit,
            refresh && !no_refresh,
            json,
            stdout,
        ),
        CloudCommand::Workflows => cloud_workflows(config, cwd, json, stdout),
    }
}

fn handoff<W: Write>(
    command: CloudHandoffCommand,
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        CloudHandoffCommand::ListStuck { threshold, repo } => {
            handoff_list_stuck(&threshold, repo, cwd, actions, json, stdout)
        }
        CloudHandoffCommand::Run {
            run_id,
            provider,
            repo,
            apply,
            dry_run,
        } => handoff_run(
            run_id, provider, repo, apply, dry_run, config, cwd, actions, json, stdout,
        ),
    }
}

fn cloud_run<W: Write>(
    args: &CloudRunArgs,
    records: &CloudRecordStore,
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let workflows = discover_workflows(cwd);
    let workflow_key = args
        .workflow_key
        .clone()
        .or_else(|| default_workflow_key(config, &workflows))
        .ok_or_else(|| CliFailure::new(1, "No workflows discovered"))?;
    let ref_name = match &args.ref_name {
        Some(ref_name) => ref_name.clone(),
        None => {
            current_git_branch(cwd).ok_or_else(|| CliFailure::new(1, "Not in a git repository"))?
        }
    };
    let plan = resolve_cloud_dispatch_plan_with_overrides(
        config,
        &workflows,
        &workflow_key,
        &ref_name,
        CloudDispatchOverrides {
            provider: args.provider.as_deref(),
            runner_selector: args.runner_selector.as_deref(),
            linux_runner_selector: args.linux_runner_selector.as_deref(),
            windows_runner_selector: args.windows_runner_selector.as_deref(),
            macos_runner_selector: args.macos_runner_selector.as_deref(),
        },
    )
    .map_err(|error| CliFailure::new(1, format!("Could not plan dispatch: {error}")))?;
    check_required_sha(args.require_sha.as_deref(), &plan, cwd, actions)?;

    let dispatch_id = records.new_dispatch_id();
    let mut record = CloudRunRecord::new(
        dispatch_id,
        plan.workflow.key.clone(),
        plan.workflow.file.clone(),
        plan.workflow.name.clone(),
        plan.ref_name.clone(),
        plan.provider.clone(),
    );
    record.repository.clone_from(&plan.repository);
    record.dispatch_fields.clone_from(&plan.dispatch_fields);

    let wait = args.wait && !args.no_wait;
    match cloud_run_dispatch(actions, &plan, args.run_id.as_deref()) {
        Ok((run_id, run_url, status)) => {
            record.run_id = Some(run_id);
            record.url = run_url;
            record.status = status;
        }
        Err(error) => {
            "error".clone_into(&mut record.status);
            record.conclusion = Some("error".to_owned());
            record.updated_at = Some(Utc::now());
            records
                .save(&record)
                .map_err(|save_error| CliFailure::new(1, save_error.to_string()))?;
            return Err(error);
        }
    }

    if wait
        && let Some(run_id) = record
            .run_id
            .as_deref()
            .and_then(|run_id| run_id.parse::<u64>().ok())
    {
        let view = wait_for_cloud_completion(actions, plan.repository.as_deref(), run_id)?;
        record.status = view.status;
        record.conclusion = view.conclusion;
        record.url = view.url.or(record.url);
        record.started_at = record.started_at.or_else(|| Some(Utc::now()));
        record.completed_at = (record.status == "completed").then(Utc::now);
        record.updated_at = Some(Utc::now());
    }

    records
        .save(&record)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    render_cloud_run(stdout, json, &record, &plan)?;
    Ok(ExitCode::SUCCESS)
}

fn cloud_run_dispatch(
    actions: &GitHubActions,
    plan: &CloudDispatchPlan,
    forced_run_id: Option<&str>,
) -> Result<(String, Option<String>, String), CliFailure> {
    if let Some(run_id) = forced_run_id {
        return Ok((run_id.to_owned(), None, "queued".to_owned()));
    }
    let previous_run_id = actions
        .latest_workflow_run_for_branch(
            plan.repository.as_deref(),
            &plan.workflow.file,
            &plan.ref_name,
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .map(|run| run.database_id);
    actions
        .workflow_dispatch(
            plan.repository.as_deref(),
            &plan.workflow.file,
            &plan.ref_name,
            &plan.dispatch_fields,
        )
        .map_err(|error| CliFailure::new(1, format!("workflow_dispatch failed: {error}")))?;
    let run = actions
        .find_dispatched_run_after(
            plan.repository.as_deref(),
            &plan.workflow.file,
            &plan.ref_name,
            previous_run_id,
            DISPATCH_DISCOVERY_TIMEOUT,
            DISPATCH_DISCOVERY_POLL,
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok((run.database_id.to_string(), run.url, run.status))
}

fn wait_for_cloud_completion(
    actions: &GitHubActions,
    repository: Option<&str>,
    run_id: u64,
) -> Result<crate::cloud::WorkflowRun, CliFailure> {
    loop {
        let view = actions
            .workflow_run_status(repository, run_id)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if view.status == "completed" {
            return Ok(view);
        }
        std::thread::sleep(DISPATCH_DISCOVERY_POLL);
    }
}

fn render_cloud_run<W: Write>(
    stdout: &mut W,
    json: bool,
    record: &CloudRunRecord,
    plan: &CloudDispatchPlan,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "record".to_owned(),
            serde_json::to_value(record).expect("record serializes"),
        );
        data.insert(
            "plan".to_owned(),
            serde_json::to_value(plan).expect("plan serializes"),
        );
        write_json_envelope(stdout, "cloud.run", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    writeln!(
        stdout,
        "Dispatched {} to {} ({})",
        record.workflow_key, record.provider, record.dispatch_id
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if let Some(run_id) = &record.run_id {
        writeln!(stdout, "run id: {run_id}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if let Some(url) = &record.url {
        writeln!(stdout, "url: {url}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn check_required_sha(
    require_sha: Option<&str>,
    plan: &CloudDispatchPlan,
    cwd: &Path,
    actions: &GitHubActions,
) -> Result<(), CliFailure> {
    let Some(require_sha) = require_sha else {
        return Ok(());
    };
    let expected = resolve_expected_sha(require_sha, cwd).ok_or_else(|| {
        CliFailure::new(
            1,
            format!(
                "Could not resolve --require-sha value '{require_sha}'. Pass an explicit 40-char SHA or 'HEAD'."
            ),
        )
    })?;
    let dispatch_repo = plan
        .repository
        .clone()
        .or_else(|| detect_repo_slug(cwd))
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "--require-sha couldn't determine the dispatch repository.",
            )
        })?;
    let remote_sha = remote_ref_sha(&dispatch_repo, &plan.ref_name, actions).ok_or_else(|| {
        CliFailure::new(
            1,
            format!(
                "Could not read remote SHA for {}@{}.",
                dispatch_repo, plan.ref_name
            ),
        )
    })?;
    if remote_sha != expected {
        return Err(CliFailure::new(
            1,
            format!(
                "Stale dispatch refused: expected {} but {}@{} is at {}. Push the expected commit, or re-run without --require-sha.",
                &expected[..12],
                dispatch_repo,
                plan.ref_name,
                &remote_sha[..12],
            ),
        ));
    }
    Ok(())
}

fn current_git_branch(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!branch.is_empty()).then_some(branch)
}

fn resolve_expected_sha(value: &str, cwd: &Path) -> Option<String> {
    if value.eq_ignore_ascii_case("HEAD") {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(cwd)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let sha = String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_lowercase();
        return valid_sha(&sha).then_some(sha);
    }
    let sha = value.trim().to_lowercase();
    valid_sha(&sha).then_some(sha)
}

fn remote_ref_sha(repo_slug: &str, ref_name: &str, actions: &GitHubActions) -> Option<String> {
    let args = vec![
        "api".to_owned(),
        format!("repos/{repo_slug}/commits/{ref_name}"),
        "--jq".to_owned(),
        ".sha".to_owned(),
    ];
    let sha = actions.run_gh(&args).ok()?.trim().to_lowercase();
    valid_sha(&sha).then_some(sha)
}

fn detect_repo_slug(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_github_repo_slug(&String::from_utf8_lossy(&output.stdout))
}

fn valid_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[allow(clippy::too_many_lines)]
fn add_lane<W: Write>(
    args: &CloudAddLaneArgs,
    store: &ShipStateStore,
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Some(state) = store.get(args.pr) else {
        return error(
            stdout,
            format!(
                "No in-flight ship state for PR #{}. add-lane operates on a running ship.",
                args.pr
            ),
        );
    };

    if ship_terminal_verdict(&state).is_some() {
        return error(
            stdout,
            format!(
                "PR #{}: ship is already past dispatch phase (evidence={:?}). Can't add a lane after merge has been issued.",
                args.pr, state.evidence_snapshot
            ),
        );
    }

    if state.has_target(&args.target) {
        let existing = state.get_run(&args.target);
        let mut data = add_lane_payload(args, "noop", true);
        data.insert("already_tracked".to_owned(), Value::Bool(true));
        data.insert(
            "existing_run".to_owned(),
            existing.map_or(Value::Null, DispatchedRun::to_json_value),
        );
        render_cloud_event(stdout, json, "cloud.add-lane", data, || {
            format!(
                "Target '{}' is already tracked in PR #{}'s dispatched_runs. No-op.",
                args.target, args.pr
            )
        })?;
        return Ok(ExitCode::SUCCESS);
    }

    let (workflow_key, plan) = resolve_lane_plan(
        config,
        cwd,
        args.workflow.as_deref(),
        &state.branch,
        args.provider.as_deref(),
    )?;
    let dispatch_repo = plan
        .repository
        .clone()
        .unwrap_or_else(|| state.repo.clone());
    let apply = args.apply && !args.dry_run;
    let mut data = add_lane_payload(args, if apply { "applied" } else { "plan" }, !apply);
    data.insert("branch".to_owned(), Value::from(state.branch.clone()));
    data.insert("repo".to_owned(), Value::from(dispatch_repo.clone()));
    data.insert("workflow_key".to_owned(), Value::from(workflow_key.clone()));
    data.insert("provider".to_owned(), Value::from(plan.provider.clone()));
    data.insert(
        "dispatch_fields".to_owned(),
        serde_json::to_value(&plan.dispatch_fields).expect("dispatch fields serialize"),
    );

    if !apply {
        render_cloud_event(stdout, json, "cloud.add-lane", data, || {
            format!(
                "Add-lane plan for PR #{}: target={} provider={}. Dry-run. Re-run with --apply to dispatch.",
                args.pr, args.target, plan.provider
            )
        })?;
        return Ok(ExitCode::SUCCESS);
    }

    let (run_id, run_url) = dispatch_and_discover(
        actions,
        &dispatch_repo,
        &plan,
        args.run_id.as_deref(),
        &args.target,
        true,
    )?;
    store
        .with_pr_state_locked(args.pr, |current| {
            let state = current
                .as_mut()
                .ok_or_else(|| format!("No in-flight ship state for PR #{}", args.pr))?;
            if ship_terminal_verdict(state).is_some() {
                return Err(format!("PR #{}: ship is already past dispatch phase", args.pr).into());
            }
            if !state.has_target(&args.target) {
                append_lane_run(
                    state,
                    &args.target,
                    &plan.provider,
                    &run_id,
                    lane_is_required(config, &args.target),
                );
            }
            Ok(())
        })
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    data.insert("run_id".to_owned(), Value::from(run_id));
    data.insert(
        "run_url".to_owned(),
        run_url.map_or(Value::Null, Value::from),
    );
    render_cloud_event(stdout, json, "cloud.add-lane", data, || {
        format!(
            "Dispatched {} on {}. Appended to PR #{} ship state.",
            args.target, plan.provider, args.pr
        )
    })?;
    Ok(ExitCode::SUCCESS)
}

fn retarget<W: Write>(
    args: &CloudRetargetArgs,
    store: &ShipStateStore,
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Some(state) = store.get(args.pr) else {
        return error(
            stdout,
            format!(
                "No in-flight ship state for PR #{}. retarget operates on a running ship.",
                args.pr
            ),
        );
    };

    if state.get_run(&args.target).is_none() {
        return error(
            stdout,
            format!(
                "No existing target '{}' in PR #{}'s dispatched_runs.",
                args.target, args.pr
            ),
        );
    }

    let context = retarget_context(args, config, cwd, actions, &state)?;
    if args.run_id.is_none() && context.matching_jobs.is_empty() {
        return error(
            stdout,
            format!(
                "No jobs matching '{}' in run {}.",
                args.target, context.run_id
            ),
        );
    }
    let apply = args.apply && !args.dry_run;
    let data = retarget_data(args, &context, apply);
    if !apply {
        render_cloud_event(stdout, json, "cloud.retarget", data, || {
            format!(
                "Retarget plan for PR #{}: target={} provider={}. Dry-run. Re-run with --apply to cancel + redispatch.",
                args.pr, args.target, args.provider
            )
        })?;
        return Ok(ExitCode::SUCCESS);
    }

    apply_retarget(
        RetargetApplyRequest {
            args,
            store,
            config,
            actions,
            context,
            data,
        },
        json,
        stdout,
    )
}

struct RetargetContext {
    workflow_key: String,
    plan: CloudDispatchPlan,
    dispatch_repo: String,
    head_ref: String,
    run_id: u64,
    active_jobs: Vec<WorkflowJob>,
    matching_jobs: Vec<WorkflowJob>,
}

fn retarget_context(
    args: &CloudRetargetArgs,
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    state: &ShipState,
) -> Result<RetargetContext, CliFailure> {
    let (workflow_key, mut plan) = resolve_lane_plan(
        config,
        cwd,
        args.workflow.as_deref(),
        &state.branch,
        Some(&args.provider),
    )?;
    let dispatch_repo = plan
        .repository
        .clone()
        .unwrap_or_else(|| state.repo.clone());
    let head_ref = retarget_head_ref(args, actions, state, &dispatch_repo)?;
    if head_ref != plan.ref_name {
        (_, plan) = resolve_lane_plan(
            config,
            cwd,
            args.workflow.as_deref(),
            &head_ref,
            Some(&args.provider),
        )?;
    }
    let (run_id, active_jobs, matching_jobs) = retarget_run_and_jobs(
        args,
        actions,
        state,
        &workflow_key,
        &dispatch_repo,
        &head_ref,
        &plan,
    )?;
    Ok(RetargetContext {
        workflow_key,
        plan,
        dispatch_repo,
        head_ref,
        run_id,
        active_jobs,
        matching_jobs,
    })
}

fn retarget_head_ref(
    args: &CloudRetargetArgs,
    actions: &GitHubActions,
    state: &ShipState,
    dispatch_repo: &str,
) -> Result<String, CliFailure> {
    if args.run_id.is_some() {
        return Ok(state.branch.clone());
    }
    actions
        .pr_head_ref(dispatch_repo, args.pr)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "PR #{}: could not fetch state in {dispatch_repo} via gh: {error}",
                    args.pr
                ),
            )
        })?
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!("PR #{}: no headRefName in gh response.", args.pr),
            )
        })
}

fn retarget_run_and_jobs(
    args: &CloudRetargetArgs,
    actions: &GitHubActions,
    state: &ShipState,
    workflow_key: &str,
    dispatch_repo: &str,
    head_ref: &str,
    plan: &CloudDispatchPlan,
) -> Result<(u64, Vec<WorkflowJob>, Vec<WorkflowJob>), CliFailure> {
    if args.run_id.is_some() {
        return Ok((
            state
                .get_run(&args.target)
                .and_then(|run| run.run_id.parse::<u64>().ok())
                .unwrap_or(0),
            Vec::new(),
            Vec::new(),
        ));
    }
    let latest = actions
        .latest_workflow_run_for_branch(Some(dispatch_repo), &plan.workflow.file, head_ref)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!(
                    "No workflow runs found for {workflow_key} on {dispatch_repo}@{head_ref}. Dispatch first, then retarget."
                ),
            )
        })?;
    let jobs = actions
        .matching_jobs(dispatch_repo, latest.database_id, &args.target)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let active_jobs = actions
        .active_jobs(dispatch_repo, latest.database_id)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok((latest.database_id, active_jobs, jobs))
}

fn retarget_data(
    args: &CloudRetargetArgs,
    context: &RetargetContext,
    apply: bool,
) -> BTreeMap<String, Value> {
    let mut data = retarget_payload(args, if apply { "applied" } else { "plan" }, !apply);
    data.insert("head_ref".to_owned(), Value::from(context.head_ref.clone()));
    data.insert(
        "repo".to_owned(),
        Value::from(context.dispatch_repo.clone()),
    );
    data.insert(
        "workflow_key".to_owned(),
        Value::from(context.workflow_key.clone()),
    );
    data.insert("run_id".to_owned(), Value::from(context.run_id));
    data.insert(
        "matching_jobs".to_owned(),
        serde_json::to_value(jobs_with_urls(
            &context.dispatch_repo,
            context.run_id,
            &context.matching_jobs,
        ))
        .expect("jobs serialize"),
    );
    data.insert(
        "new_provider".to_owned(),
        Value::from(args.provider.clone()),
    );
    data
}

struct RetargetApplyRequest<'a> {
    args: &'a CloudRetargetArgs,
    store: &'a ShipStateStore,
    config: &'a LoadedConfig,
    actions: &'a GitHubActions,
    context: RetargetContext,
    data: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CancelFailureKind {
    Auth,
    Scope,
    NotFound,
    Unsupported,
    Transient,
    Unknown,
}

impl CancelFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Scope => "scope",
            Self::NotFound => "not_found",
            Self::Unsupported => "unsupported",
            Self::Transient => "transient",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CancelFailure {
    job_id: Option<u64>,
    job_name: Option<String>,
    job_url: Option<String>,
    run_id: Option<u64>,
    run_url: Option<String>,
    kind: CancelFailureKind,
    http_status: Option<u16>,
    message: String,
}

impl CancelFailure {
    fn for_job(repo: &str, run_id: u64, job: &WorkflowJob, error: &GitHubError) -> Self {
        let message = error.message().to_owned();
        Self {
            job_id: Some(job.database_id),
            job_name: Some(job.name.clone()),
            job_url: Some(github_job_url(repo, run_id, job.database_id)),
            run_id: None,
            run_url: None,
            kind: classify_cancel_message(&message),
            http_status: http_status_from_message(&message),
            message,
        }
    }

    fn for_run(repo: &str, run_id: u64, error: &GitHubError) -> Self {
        let message = error.message().to_owned();
        Self {
            job_id: None,
            job_name: None,
            job_url: None,
            run_id: Some(run_id),
            run_url: Some(github_run_url(repo, run_id)),
            kind: classify_cancel_message(&message),
            http_status: http_status_from_message(&message),
            message,
        }
    }

    fn target_label(&self) -> String {
        if let Some(job_id) = self.job_id {
            let name = self
                .job_name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or("unnamed job");
            return format!("job {job_id} ({name})");
        }
        if let Some(run_id) = self.run_id {
            return format!("run {run_id}");
        }
        "cancellation target".to_owned()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CancelSummary {
    cancelled_job_ids: Vec<u64>,
    failures: Vec<CancelFailure>,
    used_run_fallback: bool,
}

impl CancelSummary {
    fn can_dispatch(&self, matching_jobs: &[WorkflowJob]) -> bool {
        if matching_jobs.is_empty() {
            return self.failures.is_empty();
        }
        if !self.failures.is_empty() {
            return false;
        }
        let cancelled: std::collections::BTreeSet<_> =
            self.cancelled_job_ids.iter().copied().collect();
        matching_jobs
            .iter()
            .all(|job| cancelled.contains(&job.database_id))
    }
}

fn apply_retarget<W: Write>(
    mut request: RetargetApplyRequest<'_>,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let cancel_summary = cancel_matching_jobs(
        request.actions,
        &request.context.dispatch_repo,
        request.context.run_id,
        &request.context.active_jobs,
        &request.context.matching_jobs,
    );
    if !cancel_summary.can_dispatch(&request.context.matching_jobs) {
        return render_retarget_cancel_failed(
            request.args,
            &request.context,
            request.data,
            &cancel_summary,
            json,
            stdout,
        );
    }
    let used_run_fallback = cancel_summary.used_run_fallback;
    let cancelled_job_ids = cancel_summary.cancelled_job_ids;
    let (new_run_id, _) = dispatch_and_discover(
        request.actions,
        &request.context.dispatch_repo,
        &request.context.plan,
        request.args.run_id.as_deref(),
        &request.args.target,
        false,
    )?;
    request
        .store
        .with_pr_state_locked(request.args.pr, |current| {
            let state = current
                .as_mut()
                .ok_or_else(|| format!("No in-flight ship state for PR #{}", request.args.pr))?;
            state
                .dispatched_runs
                .retain(|run| run.target != request.args.target);
            append_lane_run(
                state,
                &request.args.target,
                &request.context.plan.provider,
                &new_run_id,
                lane_is_required(request.config, &request.args.target),
            );
            Ok(())
        })
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    request
        .data
        .insert("new_run_id".to_owned(), Value::from(new_run_id));
    request.data.insert(
        "cancelled_job_ids".to_owned(),
        serde_json::to_value(&cancelled_job_ids).expect("cancelled ids serialize"),
    );
    request.data.insert(
        "run_cancel_fallback_used".to_owned(),
        Value::Bool(used_run_fallback),
    );
    request
        .data
        .insert("stale_old_blocker_remains".to_owned(), Value::Bool(false));
    request.data.insert(
        "stale_old_blocker_status".to_owned(),
        Value::from("cleared"),
    );
    request.data.insert(
        "new_dispatch".to_owned(),
        serde_json::to_value(&request.context.plan).expect("plan serialize"),
    );
    render_cloud_event(stdout, json, "cloud.retarget", request.data, || {
        format!(
            "Retargeted {} to provider={}. Updated PR #{} ship state.",
            request.args.target, request.context.plan.provider, request.args.pr
        )
    })?;
    Ok(ExitCode::SUCCESS)
}

fn render_retarget_cancel_failed<W: Write>(
    args: &CloudRetargetArgs,
    context: &RetargetContext,
    mut data: BTreeMap<String, Value>,
    summary: &CancelSummary,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    add_cancel_failed_data(&mut data, args, context, summary);
    render_cloud_event(stdout, json, "cloud.retarget", data, || {
        cancel_failed_human_message(args, context, summary)
    })?;
    Ok(ExitCode::from(1))
}

fn add_cancel_failed_data(
    data: &mut BTreeMap<String, Value>,
    args: &CloudRetargetArgs,
    context: &RetargetContext,
    summary: &CancelSummary,
) {
    let run_url = github_run_url(&context.dispatch_repo, context.run_id);
    data.insert("event".to_owned(), Value::from("cancel_failed"));
    data.insert("dry_run".to_owned(), Value::Bool(false));
    data.insert("run_url".to_owned(), Value::from(run_url.clone()));
    data.insert("manual_cancel_url".to_owned(), Value::from(run_url));
    data.insert(
        "cancelled_job_ids".to_owned(),
        serde_json::to_value(&summary.cancelled_job_ids).expect("cancelled ids serialize"),
    );
    data.insert(
        "cancel_failures".to_owned(),
        serde_json::to_value(cancel_failure_rows(summary)).expect("cancel failures serialize"),
    );
    data.insert(
        "manual_recovery_steps".to_owned(),
        serde_json::to_value(manual_recovery_steps(args, context, summary))
            .expect("recovery steps serialize"),
    );
    data.insert("additive_dispatch_supported".to_owned(), Value::Bool(false));
    data.insert(
        "additive_dispatch_warning".to_owned(),
        Value::from(additive_dispatch_warning()),
    );
    data.insert(
        "branch_protection_note".to_owned(),
        Value::from(branch_protection_note()),
    );
    data.insert(
        "run_cancel_fallback_attempted".to_owned(),
        Value::Bool(active_jobs_all_match(
            &context.active_jobs,
            &context.matching_jobs,
        )),
    );
    data.insert(
        "run_cancel_fallback_used".to_owned(),
        Value::Bool(summary.used_run_fallback),
    );
    data.insert("stale_old_blocker_remains".to_owned(), Value::Null);
    data.insert(
        "stale_old_blocker_status".to_owned(),
        Value::from("unknown_cancel_failed"),
    );
}

fn cancel_failure_rows(summary: &CancelSummary) -> Vec<BTreeMap<String, Value>> {
    summary
        .failures
        .iter()
        .map(|failure| {
            let mut row = BTreeMap::new();
            row.insert(
                "target".to_owned(),
                Value::from(if failure.job_id.is_some() {
                    "job"
                } else {
                    "run"
                }),
            );
            if let Some(job_id) = failure.job_id {
                row.insert("job_id".to_owned(), Value::from(job_id));
            }
            if let Some(name) = &failure.job_name {
                row.insert("job_name".to_owned(), Value::from(name.clone()));
            }
            if let Some(url) = &failure.job_url {
                row.insert("job_url".to_owned(), Value::from(url.clone()));
            }
            if let Some(run_id) = failure.run_id {
                row.insert("run_id".to_owned(), Value::from(run_id));
            }
            if let Some(url) = &failure.run_url {
                row.insert("run_url".to_owned(), Value::from(url.clone()));
            }
            if let Some(status) = failure.http_status {
                row.insert("http_status".to_owned(), Value::from(status));
            }
            row.insert(
                "classification".to_owned(),
                Value::from(failure.kind.as_str()),
            );
            row.insert("reason".to_owned(), Value::from(failure.kind.as_str()));
            row.insert("message".to_owned(), Value::from(failure.message.clone()));
            row
        })
        .collect()
}

fn cancel_failed_human_message(
    args: &CloudRetargetArgs,
    context: &RetargetContext,
    summary: &CancelSummary,
) -> String {
    let mut lines = vec![format!(
        "Couldn't cancel every matching job for PR #{} target={}; no replacement dispatch was sent.",
        args.pr, args.target
    )];
    lines.push(format!(
        "Run: {}",
        github_run_url(&context.dispatch_repo, context.run_id)
    ));
    if !summary.cancelled_job_ids.is_empty() {
        lines.push(format!(
            "Already cancelled job ids: {}",
            join_ids(&summary.cancelled_job_ids)
        ));
    }
    for failure in &summary.failures {
        let status = failure
            .http_status
            .map(|status| format!(" HTTP {status}"))
            .unwrap_or_default();
        lines.push(format!(
            "Cancellation failed for {}: {}{}.",
            failure.target_label(),
            failure.kind.as_str(),
            status
        ));
    }
    if needs_auth_recovery(summary) {
        lines.push("Auth recovery: run `gh auth refresh -h github.com -s workflow`, or grant Actions: Read and write on the token/App identity.".to_owned());
    }
    lines.extend(manual_recovery_steps(args, context, summary));
    lines.push(additive_dispatch_warning().to_owned());
    lines.join("\n")
}

fn manual_recovery_steps(
    args: &CloudRetargetArgs,
    context: &RetargetContext,
    summary: &CancelSummary,
) -> Vec<String> {
    let mut steps = Vec::new();
    steps.push(format!(
        "Open {} and cancel the stale target job or run manually.",
        github_run_url(&context.dispatch_repo, context.run_id)
    ));
    steps.push(format!(
        "After GitHub shows the stale target is no longer active, re-run: {}",
        retarget_apply_command(args)
    ));
    if needs_auth_recovery(summary) {
        steps.push(
            "If this is an auth/scope failure, refresh gh with `gh auth refresh -h github.com -s workflow` or grant Actions: Read and write on the token/App identity."
                .to_owned(),
        );
    }
    steps.push(branch_protection_note().to_owned());
    steps
}

fn needs_auth_recovery(summary: &CancelSummary) -> bool {
    summary.failures.iter().any(|failure| {
        matches!(
            failure.kind,
            CancelFailureKind::Auth | CancelFailureKind::Scope
        )
    })
}

fn additive_dispatch_warning() -> &'static str {
    "Shipyard did not dispatch additively because cancellation failed; a fresh workflow_dispatch may not satisfy the stale PR-event required check context."
}

fn branch_protection_note() -> &'static str {
    "Local diagnostics such as `shipyard run --targets <target>` can prove the lane locally, but they do not replace the GitHub required check context unless the workflow/check integration is updated."
}

fn retarget_apply_command(args: &CloudRetargetArgs) -> String {
    let mut parts = vec![
        "shipyard".to_owned(),
        "cloud".to_owned(),
        "retarget".to_owned(),
        "--pr".to_owned(),
        args.pr.to_string(),
        "--target".to_owned(),
        shell_quote(&args.target),
        "--provider".to_owned(),
        shell_quote(&args.provider),
    ];
    if let Some(workflow) = &args.workflow {
        parts.extend(["--workflow".to_owned(), shell_quote(workflow)]);
    }
    parts.push("--apply".to_owned());
    parts.join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/'))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn join_ids(ids: &[u64]) -> String {
    ids.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[allow(clippy::too_many_arguments)]
fn handoff_run<W: Write>(
    run_id: u64,
    provider: String,
    repo: Option<String>,
    apply: bool,
    dry_run: bool,
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repo_slug = resolve_repo_slug(repo, cwd)?;
    let metadata = actions
        .run_metadata(&repo_slug, run_id)
        .map_err(|error| CliFailure::new(1, format!("Could not fetch run {run_id}: {error}")))?;
    let workflow_file = workflow_file_from_run_path(&metadata.path)?;
    let workflows = discover_workflows(cwd);
    let Some((workflow_key, _)) = workflow_for_file(&workflows, &workflow_file) else {
        return error(
            stdout,
            format!(
                "Run {run_id} is for workflow '{workflow_file}' but no matching workflow key was discovered."
            ),
        );
    };
    let plan = resolve_cloud_dispatch_plan(
        config,
        &workflows,
        &workflow_key,
        &metadata.head_branch,
        Some(&provider),
    )
    .map_err(|error| {
        CliFailure::new(
            1,
            format!("Workflow '{workflow_key}' can't be handed off: {error}"),
        )
    })?;

    let will_apply = apply && !dry_run;
    let mut data = BTreeMap::new();
    data.insert(
        "event".to_owned(),
        Value::from(if will_apply { "applied" } else { "plan" }),
    );
    data.insert("repo".to_owned(), Value::from(repo_slug.clone()));
    data.insert("run_id".to_owned(), Value::from(run_id));
    data.insert("workflow_key".to_owned(), Value::from(workflow_key));
    data.insert("workflow_file".to_owned(), Value::from(workflow_file));
    data.insert("workflow_name".to_owned(), Value::from(metadata.name));
    data.insert("ref".to_owned(), Value::from(metadata.head_branch.clone()));
    data.insert("status".to_owned(), Value::from(metadata.status));
    data.insert("new_provider".to_owned(), Value::from(provider));
    data.insert("dry_run".to_owned(), Value::Bool(!will_apply));

    if !will_apply {
        render_cloud_event(stdout, json, "cloud.handoff", data, || {
            format!(
                "Handoff plan for run {run_id} ({repo_slug}). Dry-run. Re-run with --apply to cancel + redispatch."
            )
        })?;
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(message) = handoff_repo_mismatch_message(plan.repository.as_deref(), &repo_slug) {
        return error(stdout, message);
    }

    actions
        .cancel_workflow_run(&repo_slug, run_id)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("Could not cancel run {run_id}. Token may lack `actions:write`: {error}"),
            )
        })?;
    actions
        .workflow_dispatch(
            Some(&repo_slug),
            &plan.workflow.file,
            &plan.ref_name,
            &plan.dispatch_fields,
        )
        .map_err(|error| CliFailure::new(1, format!("workflow_dispatch failed: {error}")))?;
    data.insert("cancelled_run_id".to_owned(), Value::from(run_id));
    data.insert(
        "new_dispatch".to_owned(),
        serde_json::to_value(&plan).expect("plan serialize"),
    );
    render_cloud_event(stdout, json, "cloud.handoff", data, || {
        format!("Cancelled run {run_id}; dispatched fresh run with provider override.")
    })?;
    Ok(ExitCode::SUCCESS)
}

fn handoff_repo_mismatch_message(
    plan_repository: Option<&str>,
    source_repo: &str,
) -> Option<String> {
    let plan_repository = plan_repository?.trim();
    if plan_repository.is_empty() || plan_repository == source_repo {
        return None;
    }
    Some(format!(
        "Refusing: cloud.repository ({plan_repository}) differs from the run's source repo ({source_repo}). `handoff run` must dispatch where the cancellation happened. Use `cloud retarget` for cross-repo flows, or remove the cloud.repository override."
    ))
}

fn handoff_list_stuck<W: Write>(
    threshold: &str,
    repo: Option<String>,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let threshold_secs = parse_threshold_secs(threshold)
        .ok_or_else(|| CliFailure::new(1, format!("Bad --threshold value: {threshold:?}")))?;
    let repo_slug = resolve_repo_slug(repo, cwd)?;
    let runs = actions
        .list_queued_runs(&repo_slug, 50)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let stuck = stuck_runs(&runs, threshold_secs);

    let mut data = BTreeMap::new();
    data.insert("event".to_owned(), Value::from("list-stuck"));
    data.insert("repo".to_owned(), Value::from(repo_slug.clone()));
    data.insert("threshold_secs".to_owned(), Value::from(threshold_secs));
    data.insert(
        "stuck".to_owned(),
        serde_json::to_value(&stuck).expect("stuck rows serialize"),
    );
    render_cloud_event(stdout, json, "cloud.handoff", data, || {
        if stuck.is_empty() {
            format!("No queued runs older than {threshold} on {repo_slug}.")
        } else {
            format!("Stuck queued runs on {repo_slug}: {}", stuck.len())
        }
    })?;
    Ok(ExitCode::SUCCESS)
}

fn resolve_lane_plan(
    config: &LoadedConfig,
    cwd: &Path,
    workflow_override: Option<&str>,
    ref_name: &str,
    provider_override: Option<&str>,
) -> Result<(String, CloudDispatchPlan), CliFailure> {
    let workflows = discover_workflows(cwd);
    let workflow_key = workflow_override
        .map(ToOwned::to_owned)
        .or_else(|| default_workflow_key(config, &workflows))
        .ok_or_else(|| CliFailure::new(1, "No workflows discovered"))?;
    let plan = resolve_cloud_dispatch_plan(
        config,
        &workflows,
        &workflow_key,
        ref_name,
        provider_override,
    )
    .map_err(|error| CliFailure::new(1, format!("Could not plan dispatch: {error}")))?;
    Ok((workflow_key, plan))
}

fn dispatch_and_discover(
    actions: &GitHubActions,
    dispatch_repo: &str,
    plan: &CloudDispatchPlan,
    forced_run_id: Option<&str>,
    target: &str,
    tolerate_discovery_failure: bool,
) -> Result<(String, Option<String>), CliFailure> {
    if let Some(run_id) = forced_run_id {
        return Ok((run_id.to_owned(), None));
    }

    let previous_run_id = match actions.latest_workflow_run_for_branch(
        Some(dispatch_repo),
        &plan.workflow.file,
        &plan.ref_name,
    ) {
        Ok(run) => run.map(|run| run.database_id),
        Err(_error) if tolerate_discovery_failure => None,
        Err(error) => return Err(CliFailure::new(1, error.to_string())),
    };

    actions
        .workflow_dispatch(
            Some(dispatch_repo),
            &plan.workflow.file,
            &plan.ref_name,
            &plan.dispatch_fields,
        )
        .map_err(|error| CliFailure::new(1, format!("workflow_dispatch failed: {error}")))?;
    match actions.find_dispatched_run_after(
        Some(dispatch_repo),
        &plan.workflow.file,
        &plan.ref_name,
        previous_run_id,
        DISPATCH_DISCOVERY_TIMEOUT,
        DISPATCH_DISCOVERY_POLL,
    ) {
        Ok(run) => Ok((run.database_id.to_string(), run.url)),
        Err(_error) if tolerate_discovery_failure => Ok((format!("pending-{target}"), None)),
        Err(error) => Err(CliFailure::new(1, error.to_string())),
    }
}

fn cancel_matching_jobs(
    actions: &GitHubActions,
    dispatch_repo: &str,
    run_id: u64,
    active_jobs: &[WorkflowJob],
    matching_jobs: &[WorkflowJob],
) -> CancelSummary {
    let mut summary = CancelSummary::default();
    if matching_jobs.is_empty() {
        return summary;
    }
    for job in matching_jobs {
        match actions.cancel_workflow_job(dispatch_repo, job.database_id) {
            Ok(()) => summary.cancelled_job_ids.push(job.database_id),
            Err(error) => {
                summary
                    .failures
                    .push(CancelFailure::for_job(dispatch_repo, run_id, job, &error));
            }
        }
    }
    if summary.cancelled_job_ids.is_empty() && active_jobs_all_match(active_jobs, matching_jobs) {
        match actions.cancel_workflow_run(dispatch_repo, run_id) {
            Ok(()) => {
                summary.used_run_fallback = true;
                summary.failures.clear();
                summary.cancelled_job_ids =
                    matching_jobs.iter().map(|job| job.database_id).collect();
            }
            Err(error) => {
                summary
                    .failures
                    .push(CancelFailure::for_run(dispatch_repo, run_id, &error));
            }
        }
    }
    summary
}

fn active_jobs_all_match(active_jobs: &[WorkflowJob], matching_jobs: &[WorkflowJob]) -> bool {
    if active_jobs.is_empty() {
        return false;
    }
    let active_ids: std::collections::BTreeSet<_> =
        active_jobs.iter().map(|job| job.database_id).collect();
    let matching_ids: std::collections::BTreeSet<_> =
        matching_jobs.iter().map(|job| job.database_id).collect();
    active_ids == matching_ids
}

fn jobs_with_urls(repo: &str, run_id: u64, jobs: &[WorkflowJob]) -> Vec<BTreeMap<String, Value>> {
    jobs.iter()
        .map(|job| {
            let mut row = BTreeMap::new();
            row.insert("database_id".to_owned(), Value::from(job.database_id));
            row.insert("name".to_owned(), Value::from(job.name.clone()));
            row.insert(
                "url".to_owned(),
                Value::from(github_job_url(repo, run_id, job.database_id)),
            );
            row
        })
        .collect()
}

fn github_run_url(repo: &str, run_id: u64) -> String {
    format!("https://github.com/{repo}/actions/runs/{run_id}")
}

fn github_job_url(repo: &str, run_id: u64, job_id: u64) -> String {
    format!("{}/job/{job_id}", github_run_url(repo, run_id))
}

fn classify_cancel_message(message: &str) -> CancelFailureKind {
    let status = http_status_from_message(message);
    let lower = message.to_lowercase();
    if status == Some(401)
        || lower.contains("not logged in")
        || lower.contains("authentication")
        || lower.contains("bad credentials")
        || lower.contains("requires authentication")
    {
        return CancelFailureKind::Auth;
    }
    if status == Some(429)
        || status.is_some_and(|code| (500..=599).contains(&code))
        || lower.contains("rate limit")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
    {
        return CancelFailureKind::Transient;
    }
    if status == Some(403)
        || lower.contains("actions:write")
        || lower.contains("workflow scope")
        || lower.contains("workflow' scope")
        || lower.contains("resource not accessible by integration")
        || lower.contains("permission")
    {
        return CancelFailureKind::Scope;
    }
    if status == Some(404) || lower.contains("not found") {
        return CancelFailureKind::NotFound;
    }
    if lower.contains("unsupported")
        || lower.contains("cannot cancel")
        || lower.contains("can't cancel")
        || lower.contains("already completed")
        || lower.contains("not in progress")
    {
        return CancelFailureKind::Unsupported;
    }
    CancelFailureKind::Unknown
}

fn http_status_from_message(message: &str) -> Option<u16> {
    let lower = message.to_lowercase();
    for marker in ["http/2.0 ", "http/2 ", "http/1.1 ", "http "] {
        let Some(index) = lower.find(marker) else {
            continue;
        };
        let tail = &lower[index + marker.len()..];
        let digits = tail
            .chars()
            .skip_while(|ch| !ch.is_ascii_digit())
            .take_while(char::is_ascii_digit)
            .take(3)
            .collect::<String>();
        if digits.len() == 3 {
            return digits.parse().ok();
        }
    }
    None
}

fn append_lane_run(
    state: &mut ShipState,
    target: &str,
    provider: &str,
    run_id: &str,
    required: bool,
) {
    let now = Utc::now();
    state.append_run(DispatchedRun {
        target: target.to_owned(),
        provider: provider.to_owned(),
        run_id: run_id.to_owned(),
        status: "queued".to_owned(),
        started_at: now,
        updated_at: now,
        attempt: state.attempt,
        last_heartbeat_at: None,
        phase: None,
        required,
    });
}

fn parse_threshold_secs(raw: &str) -> Option<i64> {
    let raw = raw.trim().to_lowercase();
    if raw.is_empty() {
        return None;
    }
    if let Some(hours) = raw.strip_suffix('h') {
        return hours.parse::<i64>().ok().map(|value| value * 3_600);
    }
    if let Some(minutes) = raw.strip_suffix('m') {
        return minutes.parse::<i64>().ok().map(|value| value * 60);
    }
    if let Some(seconds) = raw.strip_suffix('s') {
        return seconds.parse::<i64>().ok();
    }
    raw.parse::<i64>().ok()
}

fn stuck_runs(runs: &[QueuedRun], threshold_secs: i64) -> Vec<BTreeMap<String, Value>> {
    let now = Utc::now();
    let mut stuck = Vec::new();
    for run in runs {
        let Ok(created) = DateTime::parse_from_rfc3339(&run.created_at) else {
            continue;
        };
        let age_secs = (now - created.with_timezone(&Utc)).num_seconds();
        if age_secs < threshold_secs {
            continue;
        }
        let mut row = BTreeMap::new();
        row.insert("run_id".to_owned(), Value::from(run.database_id));
        row.insert(
            "workflow".to_owned(),
            Value::from(run.workflow_name.clone()),
        );
        row.insert("branch".to_owned(), Value::from(run.head_branch.clone()));
        row.insert("queued_for_secs".to_owned(), Value::from(age_secs));
        row.insert(
            "url".to_owned(),
            run.url.clone().map_or(Value::Null, Value::from),
        );
        stuck.push(row);
    }
    stuck
}

fn resolve_repo_slug(repo: Option<String>, cwd: &Path) -> Result<String, CliFailure> {
    if let Some(repo) = repo.filter(|value| !value.is_empty()) {
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

fn workflow_file_from_run_path(path: &str) -> Result<String, CliFailure> {
    Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| CliFailure::new(1, format!("Unexpected workflow path for run: {path:?}")))
}

fn workflow_for_file(
    workflows: &BTreeMap<String, WorkflowDefinition>,
    workflow_file: &str,
) -> Option<(String, WorkflowDefinition)> {
    workflows
        .iter()
        .find(|(_, workflow)| workflow.file == workflow_file)
        .map(|(key, workflow)| (key.clone(), workflow.clone()))
}

fn add_lane_payload(
    args: &CloudAddLaneArgs,
    event: &str,
    dry_run: bool,
) -> BTreeMap<String, Value> {
    let mut data = BTreeMap::new();
    data.insert("event".to_owned(), Value::from(event.to_owned()));
    data.insert("pr".to_owned(), Value::from(args.pr));
    data.insert("target".to_owned(), Value::from(args.target.clone()));
    data.insert(
        "provider".to_owned(),
        args.provider.clone().map_or(Value::Null, Value::from),
    );
    data.insert("dry_run".to_owned(), Value::Bool(dry_run));
    data
}

fn retarget_payload(
    args: &CloudRetargetArgs,
    event: &str,
    dry_run: bool,
) -> BTreeMap<String, Value> {
    let mut data = BTreeMap::new();
    data.insert("event".to_owned(), Value::from(event.to_owned()));
    data.insert("pr".to_owned(), Value::from(args.pr));
    data.insert("target".to_owned(), Value::from(args.target.clone()));
    data.insert("provider".to_owned(), Value::from(args.provider.clone()));
    data.insert("dry_run".to_owned(), Value::Bool(dry_run));
    data
}

fn render_cloud_event<W: Write>(
    stdout: &mut W,
    json: bool,
    command: &str,
    data: BTreeMap<String, Value>,
    human: impl FnOnce() -> String,
) -> Result<(), CliFailure> {
    if json {
        write_json_envelope(stdout, command, data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    writeln!(stdout, "{}", human()).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn error<W: Write>(
    stdout: &mut W,
    message: impl std::fmt::Display,
) -> Result<ExitCode, CliFailure> {
    writeln!(stdout, "{message}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::from(1))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::{Command, ExitCode};

    use chrono::{Duration as ChronoDuration, Utc};
    use serde_json::Value;

    use super::{
        CancelFailure, CancelFailureKind, CancelSummary, RetargetContext, active_jobs_all_match,
        add_cancel_failed_data, add_lane, add_lane_payload, append_lane_run,
        cancel_failed_human_message, cancel_matching_jobs, classify_cancel_message,
        current_git_branch, detect_repo_slug, error, handoff_list_stuck,
        handoff_repo_mismatch_message, http_status_from_message, parse_threshold_secs,
        render_cloud_event, resolve_expected_sha, resolve_lane_plan, resolve_repo_slug, retarget,
        retarget_data, retarget_payload, stuck_runs, valid_sha, workflow_file_from_run_path,
        workflow_for_file,
    };
    use crate::app::cli::{CloudAddLaneArgs, CloudRetargetArgs};
    use crate::cloud::{
        CloudDispatchPlan, GitHubActions, GitHubError, QueuedRun, WorkflowDefinition, WorkflowJob,
    };
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use crate::ship_state::{ShipState, ShipStateStore};

    #[test]
    fn require_sha_helpers_match_python_contract() {
        let sha = "abcdef1234567890abcdef1234567890abcdef12";

        assert!(valid_sha(sha));
        assert_eq!(
            resolve_expected_sha(&sha.to_uppercase(), Path::new(".")),
            Some(sha.to_owned())
        );
        assert_eq!(resolve_expected_sha("abc123", Path::new(".")), None);
        assert_eq!(resolve_expected_sha(&"g".repeat(40), Path::new(".")), None);
    }

    #[test]
    fn git_context_helpers_detect_branch_and_repo_slug() {
        let temp = tempfile::tempdir().expect("tempdir");

        git(temp.path(), &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(temp.path().join("README.md"), "seed\n").expect("seed");
        git(temp.path(), &["add", "."]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "seed",
            ],
        );
        git(
            temp.path(),
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:danielraffel/pulp.git",
            ],
        );

        assert_eq!(current_git_branch(temp.path()).as_deref(), Some("main"));
        assert_eq!(
            detect_repo_slug(temp.path()).as_deref(),
            Some("danielraffel/pulp")
        );
    }

    #[test]
    fn handoff_repo_mismatch_refuses_cross_repo_dispatch() {
        let message =
            handoff_repo_mismatch_message(Some("other/repo"), "owner/source").expect("mismatch");

        assert!(message.contains("cloud.repository (other/repo)"));
        assert!(message.contains("run's source repo (owner/source)"));
        assert!(message.contains("handoff run"));
        assert!(message.contains("cloud retarget"));
    }

    #[test]
    fn handoff_repo_mismatch_allows_same_or_unspecified_repo() {
        assert!(handoff_repo_mismatch_message(None, "owner/source").is_none());
        assert!(handoff_repo_mismatch_message(Some(""), "owner/source").is_none());
        assert!(handoff_repo_mismatch_message(Some("owner/source"), "owner/source").is_none());
    }

    #[test]
    fn add_lane_reports_missing_or_terminal_state_without_github_calls() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let config = empty_config(temp.path());
        let actions = GitHubActions::new(temp.path());
        let args = add_lane_args(42, "windows");
        let mut missing_out = Vec::new();

        let code = add_lane(
            &args,
            &store,
            &config,
            temp.path(),
            &actions,
            false,
            &mut missing_out,
        )
        .expect("missing state should render");

        assert_eq!(code, ExitCode::from(1));
        assert!(
            String::from_utf8(missing_out)
                .expect("utf8")
                .contains("No in-flight ship state for PR #42")
        );

        let mut state = ShipState::new(42, "owner/repo", "feature/x", "main", "a".repeat(40), "p");
        state.dispatched_runs.push(run("linux", true));
        state
            .evidence_snapshot
            .insert("linux".to_owned(), "pass".to_owned());
        store.save(&state).expect("state");
        let mut terminal_out = Vec::new();

        let code = add_lane(
            &args,
            &store,
            &config,
            temp.path(),
            &actions,
            false,
            &mut terminal_out,
        )
        .expect("terminal state should render");

        assert_eq!(code, ExitCode::from(1));
        assert!(
            String::from_utf8(terminal_out)
                .expect("utf8")
                .contains("ship is already past dispatch phase")
        );
    }

    #[test]
    fn retarget_reports_missing_state_or_missing_target_without_github_calls() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let config = empty_config(temp.path());
        let actions = GitHubActions::new(temp.path());
        let args = retarget_args();
        let mut missing_out = Vec::new();

        let code = retarget(
            &args,
            &store,
            &config,
            temp.path(),
            &actions,
            false,
            &mut missing_out,
        )
        .expect("missing state should render");

        assert_eq!(code, ExitCode::from(1));
        assert!(
            String::from_utf8(missing_out)
                .expect("utf8")
                .contains("No in-flight ship state for PR #42")
        );

        let mut state = ShipState::new(42, "owner/repo", "feature/x", "main", "a".repeat(40), "p");
        state.dispatched_runs.push(run("linux", true));
        store.save(&state).expect("state");
        let mut missing_target_out = Vec::new();

        let code = retarget(
            &args,
            &store,
            &config,
            temp.path(),
            &actions,
            false,
            &mut missing_target_out,
        )
        .expect("missing target should render");

        assert_eq!(code, ExitCode::from(1));
        assert!(
            String::from_utf8(missing_target_out)
                .expect("utf8")
                .contains("No existing target 'windows'")
        );
    }

    #[test]
    fn handoff_list_stuck_rejects_bad_threshold_before_repo_or_github() {
        let temp = tempfile::tempdir().expect("tempdir");
        let actions = GitHubActions::new(temp.path());
        let mut out = Vec::new();

        let err = handoff_list_stuck("later", None, temp.path(), &actions, true, &mut out)
            .expect_err("bad threshold");

        assert_eq!(err.code, 1);
        assert!(err.message.contains("Bad --threshold value"));
        assert!(out.is_empty());
    }

    #[test]
    fn resolve_lane_plan_reports_missing_workflows() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = empty_config(temp.path());

        let err = resolve_lane_plan(&config, temp.path(), None, "main", None)
            .expect_err("missing workflows");

        assert_eq!(err.code, 1);
        assert_eq!(err.message, "No workflows discovered");
    }

    #[test]
    fn cancel_matching_jobs_empty_list_is_noop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let actions = GitHubActions::new(temp.path());

        let summary = cancel_matching_jobs(&actions, "owner/repo", 123, &[], &[]);

        assert!(summary.cancelled_job_ids.is_empty());
        assert!(summary.failures.is_empty());
        assert!(summary.can_dispatch(&[]));
    }

    #[test]
    fn active_job_fallback_only_applies_when_every_active_job_matches() {
        let matching = vec![WorkflowJob {
            database_id: 10,
            name: "Cloud Live Smoke".to_owned(),
        }];
        assert!(active_jobs_all_match(&matching, &matching));

        let unrelated = vec![
            WorkflowJob {
                database_id: 10,
                name: "Cloud Live Smoke".to_owned(),
            },
            WorkflowJob {
                database_id: 11,
                name: "Linux".to_owned(),
            },
        ];
        assert!(!active_jobs_all_match(&unrelated, &matching));
        assert!(!active_jobs_all_match(&[], &matching));
    }

    #[test]
    fn cancel_error_classification_keeps_scope_and_not_found_distinct() {
        assert_eq!(
            http_status_from_message("gh api failed: Not Found (HTTP 404)"),
            Some(404)
        );
        assert_eq!(http_status_from_message("HTTP/2 403 Forbidden"), Some(403));
        assert_eq!(
            classify_cancel_message("gh api failed: Not Found (HTTP 404)"),
            CancelFailureKind::NotFound
        );
        assert_eq!(
            classify_cancel_message("gh api failed: Forbidden (HTTP 403)"),
            CancelFailureKind::Scope
        );
        assert_eq!(
            classify_cancel_message("gh api failed: Bad credentials (HTTP 401)"),
            CancelFailureKind::Auth
        );
        assert_eq!(
            classify_cancel_message("gh api failed: secondary rate limit (HTTP 403)"),
            CancelFailureKind::Transient
        );
        assert_eq!(
            classify_cancel_message("gh api failed: cannot cancel completed job"),
            CancelFailureKind::Unsupported
        );
    }

    #[test]
    fn cancel_failed_payload_includes_manual_recovery_without_false_scope_hint() {
        let args = retarget_args();
        let context = retarget_context_fixture();
        let error = GitHubError::new("gh api failed: Not Found (HTTP 404)");
        let summary = CancelSummary {
            cancelled_job_ids: Vec::new(),
            failures: vec![CancelFailure::for_job(
                &context.dispatch_repo,
                context.run_id,
                &context.matching_jobs[0],
                &error,
            )],
            used_run_fallback: false,
        };
        let mut data = retarget_data(&args, &context, true);

        add_cancel_failed_data(&mut data, &args, &context, &summary);

        assert_eq!(data["event"], "cancel_failed");
        assert_eq!(data["dry_run"], false);
        assert_eq!(
            data["manual_cancel_url"],
            "https://github.com/danielraffel/pulp/actions/runs/1234"
        );
        assert_eq!(data["cancel_failures"][0]["job_id"], 555);
        assert_eq!(data["cancel_failures"][0]["classification"], "not_found");
        assert_eq!(data["cancel_failures"][0]["http_status"], 404);
        assert_eq!(
            data["cancel_failures"][0]["job_url"],
            "https://github.com/danielraffel/pulp/actions/runs/1234/job/555"
        );
        assert_eq!(data["stale_old_blocker_remains"], Value::Null);
        assert_eq!(data["stale_old_blocker_status"], "unknown_cancel_failed");
        assert_eq!(data["additive_dispatch_supported"], false);
        let steps = data["manual_recovery_steps"]
            .as_array()
            .expect("recovery steps");
        assert!(steps.iter().all(|step| {
            !step
                .as_str()
                .unwrap_or_default()
                .contains("gh auth refresh")
        }));
    }

    #[test]
    fn cancel_failed_payload_adds_scope_recovery_only_for_auth_or_scope_errors() {
        let args = retarget_args();
        let context = retarget_context_fixture();
        let error =
            GitHubError::new("gh api failed: Resource not accessible by integration (HTTP 403)");
        let summary = CancelSummary {
            cancelled_job_ids: vec![777],
            failures: vec![CancelFailure::for_job(
                &context.dispatch_repo,
                context.run_id,
                &context.matching_jobs[0],
                &error,
            )],
            used_run_fallback: false,
        };
        let mut data = retarget_data(&args, &context, true);

        add_cancel_failed_data(&mut data, &args, &context, &summary);
        let human = cancel_failed_human_message(&args, &context, &summary);

        assert_eq!(data["cancel_failures"][0]["classification"], "scope");
        assert!(human.contains("gh auth refresh -h github.com -s workflow"));
        assert!(human.contains("Already cancelled job ids: 777"));
        assert!(human.contains("no replacement dispatch was sent"));
    }

    #[test]
    fn threshold_parser_accepts_seconds_minutes_hours_and_rejects_bad_values() {
        assert_eq!(parse_threshold_secs("90"), Some(90));
        assert_eq!(parse_threshold_secs("30s"), Some(30));
        assert_eq!(parse_threshold_secs("15m"), Some(900));
        assert_eq!(parse_threshold_secs("2h"), Some(7_200));
        assert_eq!(parse_threshold_secs(" 5M "), Some(300));
        assert_eq!(parse_threshold_secs(""), None);
        assert_eq!(parse_threshold_secs("later"), None);
    }

    #[test]
    fn stuck_runs_filters_by_age_and_valid_timestamps() {
        let old = (Utc::now() - ChronoDuration::seconds(120)).to_rfc3339();
        let fresh = (Utc::now() - ChronoDuration::seconds(10)).to_rfc3339();
        let runs = vec![
            queued_run(101, "CI", "feature/old", &old),
            queued_run(102, "CI", "feature/fresh", &fresh),
            queued_run(103, "CI", "feature/bad", "not-a-timestamp"),
        ];

        let stuck = stuck_runs(&runs, 60);

        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0]["run_id"], 101);
        assert_eq!(stuck[0]["workflow"], "CI");
        assert_eq!(stuck[0]["branch"], "feature/old");
        assert!(stuck[0]["queued_for_secs"].as_i64().expect("age") >= 60);
        assert_eq!(
            stuck[0]["url"],
            "https://github.com/owner/repo/actions/runs/101"
        );
    }

    #[test]
    fn resolve_repo_slug_uses_explicit_repo_or_git_remote() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            resolve_repo_slug(Some("owner/explicit".to_owned()), temp.path())
                .expect("explicit repo"),
            "owner/explicit"
        );

        git(temp.path(), &["init", "--quiet", "--initial-branch=main"]);
        git(
            temp.path(),
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:danielraffel/pulp.git",
            ],
        );

        assert_eq!(
            resolve_repo_slug(None, temp.path()).expect("remote repo"),
            "danielraffel/pulp"
        );
    }

    #[test]
    fn resolve_repo_slug_reports_missing_context() {
        let temp = tempfile::tempdir().expect("tempdir");

        let err = resolve_repo_slug(None, temp.path()).expect_err("missing repo");

        assert_eq!(err.code, 1);
        assert!(err.message.contains("No repo detected"));
        assert!(err.message.contains("--repo OWNER/REPO"));
    }

    #[test]
    fn workflow_file_helpers_extract_and_match_files() {
        assert_eq!(
            workflow_file_from_run_path(".github/workflows/ci.yml").expect("workflow file"),
            "ci.yml"
        );
        assert!(workflow_file_from_run_path("").is_err());

        let mut workflows = BTreeMap::new();
        workflows.insert("ci".to_owned(), workflow("ci", "ci.yml"));
        workflows.insert("release".to_owned(), workflow("release", "release.yml"));

        let (key, workflow) = workflow_for_file(&workflows, "release.yml").expect("workflow");
        assert_eq!(key, "release");
        assert_eq!(workflow.file, "release.yml");
        assert!(workflow_for_file(&workflows, "missing.yml").is_none());
    }

    #[test]
    fn add_lane_and_retarget_payloads_use_stable_shapes() {
        let add = CloudAddLaneArgs {
            pr: 42,
            target: "linux".to_owned(),
            provider: Some("namespace".to_owned()),
            workflow: Some("ci".to_owned()),
            apply: false,
            dry_run: true,
            run_id: None,
        };
        let add_payload = add_lane_payload(&add, "plan", true);
        assert_eq!(add_payload["event"], "plan");
        assert_eq!(add_payload["pr"], 42);
        assert_eq!(add_payload["target"], "linux");
        assert_eq!(add_payload["provider"], "namespace");
        assert_eq!(add_payload["dry_run"], true);

        let retarget = retarget_args();
        let retarget_payload = retarget_payload(&retarget, "applied", false);
        assert_eq!(retarget_payload["event"], "applied");
        assert_eq!(retarget_payload["pr"], 42);
        assert_eq!(retarget_payload["target"], "windows");
        assert_eq!(retarget_payload["provider"], "namespace");
        assert_eq!(retarget_payload["dry_run"], false);
    }

    #[test]
    fn retarget_data_includes_context_and_matching_jobs() {
        let args = retarget_args();
        let context = retarget_context_fixture();

        let data = retarget_data(&args, &context, false);

        assert_eq!(data["event"], "plan");
        assert_eq!(data["dry_run"], true);
        assert_eq!(data["repo"], "danielraffel/pulp");
        assert_eq!(data["workflow_key"], "ci");
        assert_eq!(data["head_ref"], "feature/test");
        assert_eq!(data["run_id"], 1234);
        assert_eq!(data["matching_jobs"][0]["database_id"], 555);
        assert_eq!(
            data["matching_jobs"][0]["url"],
            "https://github.com/danielraffel/pulp/actions/runs/1234/job/555"
        );
        assert_eq!(data["new_provider"], "namespace");
    }

    #[test]
    fn render_cloud_event_supports_json_and_human_output() {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("plan"));
        data.insert("run_id".to_owned(), Value::from(100));
        let mut json_out = Vec::new();

        render_cloud_event(&mut json_out, true, "cloud.test", data, || {
            "unused human".to_owned()
        })
        .expect("json render");

        let payload: Value = serde_json::from_slice(&json_out).expect("json payload");
        assert_eq!(payload["command"], "cloud.test");
        assert_eq!(payload["event"], "plan");
        assert_eq!(payload["run_id"], 100);

        let mut human_out = Vec::new();
        render_cloud_event(&mut human_out, false, "cloud.test", BTreeMap::new(), || {
            "human message".to_owned()
        })
        .expect("human render");
        assert_eq!(
            String::from_utf8(human_out).expect("utf8"),
            "human message\n"
        );
    }

    #[test]
    fn error_writes_message_and_returns_failure_exit_code() {
        let mut out = Vec::new();

        let code = error(&mut out, "cloud failure").expect("error render");

        assert_eq!(code, ExitCode::from(1));
        assert_eq!(String::from_utf8(out).expect("utf8"), "cloud failure\n");
    }

    #[test]
    fn append_lane_run_records_queued_run_metadata() {
        let mut state = ShipState::new(
            42,
            "danielraffel/pulp",
            "feature/test",
            "main",
            "a".repeat(40),
            "policy",
        );
        state.attempt = 4;

        append_lane_run(&mut state, "windows", "namespace", "run-42", false);

        let run = state.get_run("windows").expect("run");
        assert_eq!(run.target, "windows");
        assert_eq!(run.provider, "namespace");
        assert_eq!(run.run_id, "run-42");
        assert_eq!(run.status, "queued");
        assert_eq!(run.attempt, 4);
        assert!(!run.required);
    }

    fn queued_run(id: u64, workflow: &str, branch: &str, created_at: &str) -> QueuedRun {
        QueuedRun {
            database_id: id,
            name: format!("{workflow} run"),
            head_branch: branch.to_owned(),
            created_at: created_at.to_owned(),
            workflow_name: workflow.to_owned(),
            url: Some(format!("https://github.com/owner/repo/actions/runs/{id}")),
            path: ".github/workflows/ci.yml".to_owned(),
            status: "queued".to_owned(),
            conclusion: None,
        }
    }

    fn workflow(key: &str, file: &str) -> WorkflowDefinition {
        WorkflowDefinition {
            key: key.to_owned(),
            file: file.to_owned(),
            name: key.to_owned(),
            description: String::new(),
            inputs: Vec::new(),
        }
    }

    fn dispatch_plan() -> CloudDispatchPlan {
        let mut dispatch_fields = BTreeMap::new();
        dispatch_fields.insert("runner_provider".to_owned(), "namespace".to_owned());
        CloudDispatchPlan {
            workflow: workflow("ci", "ci.yml"),
            repository: Some("danielraffel/pulp".to_owned()),
            ref_name: "feature/test".to_owned(),
            provider: "namespace".to_owned(),
            dispatch_fields,
            sources: BTreeMap::new(),
        }
    }

    fn retarget_context_fixture() -> RetargetContext {
        RetargetContext {
            workflow_key: "ci".to_owned(),
            plan: dispatch_plan(),
            dispatch_repo: "danielraffel/pulp".to_owned(),
            head_ref: "feature/test".to_owned(),
            run_id: 1234,
            active_jobs: vec![WorkflowJob {
                database_id: 555,
                name: "windows / test".to_owned(),
            }],
            matching_jobs: vec![WorkflowJob {
                database_id: 555,
                name: "windows / test".to_owned(),
            }],
        }
    }

    fn retarget_args() -> CloudRetargetArgs {
        CloudRetargetArgs {
            pr: 42,
            target: "windows".to_owned(),
            provider: "namespace".to_owned(),
            workflow: Some("ci".to_owned()),
            apply: false,
            dry_run: true,
            run_id: None,
        }
    }

    fn add_lane_args(pr: u64, target: &str) -> CloudAddLaneArgs {
        CloudAddLaneArgs {
            pr,
            target: target.to_owned(),
            provider: Some("namespace".to_owned()),
            workflow: Some("ci".to_owned()),
            apply: false,
            dry_run: true,
            run_id: None,
        }
    }

    fn run(target: &str, required: bool) -> crate::ship_state::DispatchedRun {
        let now = Utc::now();
        crate::ship_state::DispatchedRun {
            target: target.to_owned(),
            provider: "namespace".to_owned(),
            run_id: format!("run-{target}"),
            status: "queued".to_owned(),
            started_at: now,
            updated_at: now,
            attempt: 1,
            last_heartbeat_at: None,
            phase: None,
            required,
        }
    }

    fn empty_config(root: &Path) -> LoadedConfig {
        LoadedConfig {
            data: toml::Table::new(),
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("git should run");
        assert!(
            status.success(),
            "git failed in {}: {args:?}",
            cwd.display()
        );
    }
}
