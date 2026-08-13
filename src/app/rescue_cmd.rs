//! `shipyard rescue` — one-shot wedged-runner recovery.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{CliFailure, cli::RescueArgs, wait_cmd::parse_github_repo_slug};
use crate::cloud::{GitHubActions, QueuedRun, discover_workflows, resolve_cloud_dispatch_plan};
use crate::config::LoadedConfig;
use crate::output::write_json_envelope;
use crate::workflow_cancellation::is_bulk_run_cancellation_safe;

/// Cap on the number of paginated `gh api` calls per rescue invocation.
/// Each page is 100 items, so the worst case is 500 queued + 500 completed
/// runs scanned. In practice we expect early termination on the first
/// short page.
const RESCUE_LIST_MAX_PAGES: u32 = 5;
const RESCUE_EVENT: &str = "cloud.rescue";

/// Outcome of attempting to rescue a single workflow run.
#[derive(Debug, Eq, PartialEq)]
enum RunOutcome {
    /// Dry-run preview; nothing dispatched.
    Planned,
    /// Cancelled + redispatched on the new provider.
    Applied,
    /// Re-armed via `gh run rerun --failed`, then handed off.
    RerunAndApplied,
    /// Skipped: completed/cancelled run encountered without `--rerun-failed`.
    SkippedCompleted,
    /// Skipped: rescue could not plan a dispatch (e.g. workflow not found locally).
    SkippedNoPlan(String),
    /// Run was processed but a step failed.
    Failed(String),
}

impl RunOutcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Applied => "applied",
            Self::RerunAndApplied => "rerun+applied",
            Self::SkippedCompleted => "skipped-completed",
            Self::SkippedNoPlan(_) => "skipped-no-plan",
            Self::Failed(_) => "failed",
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            Self::SkippedNoPlan(message) | Self::Failed(message) => Some(message.clone()),
            _ => None,
        }
    }
}

/// Entry point invoked from `app::dispatch`.
pub(super) fn rescue_command<W: Write>(
    args: &RescueArgs,
    config: &LoadedConfig,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let actions = GitHubActions::from_loaded_config(cwd, config);
    rescue_with_actions(args, config, cwd, &actions, json, stdout, Utc::now())
}

pub(super) fn rescue_with_actions<W: Write>(
    args: &RescueArgs,
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
    now: DateTime<Utc>,
) -> Result<ExitCode, CliFailure> {
    let threshold_secs = parse_threshold_secs(&args.threshold).ok_or_else(|| {
        CliFailure::new(1, format!("Bad --threshold value: {:?}", args.threshold))
    })?;
    let repo_slug = resolve_repo_slug(args.repo.clone(), cwd)?;
    let branch = resolve_branch(args, actions, &repo_slug)?;
    let candidates = collect_candidates(
        args,
        actions,
        &repo_slug,
        branch.as_deref(),
        threshold_secs,
        now,
    )?;

    let workflows = discover_workflows(cwd);
    let mut rows: Vec<BTreeMap<String, Value>> = Vec::with_capacity(candidates.len());
    let mut any_failure = false;
    for candidate in &candidates {
        let outcome = process_candidate(candidate, args, &workflows, config, actions, &repo_slug);
        if matches!(outcome, RunOutcome::Failed(_)) {
            any_failure = true;
        }
        rows.push(candidate_row(candidate, &outcome));
    }

    let data = rescue_envelope_data(
        args,
        &repo_slug,
        branch.as_deref(),
        threshold_secs,
        &candidates,
        &rows,
    );
    render(stdout, json, data, || {
        render_human_summary(args, &repo_slug, branch.as_deref(), &rows)
    })?;

    if any_failure {
        return Ok(ExitCode::from(1));
    }
    Ok(ExitCode::SUCCESS)
}

fn resolve_branch(
    args: &RescueArgs,
    actions: &GitHubActions,
    repo_slug: &str,
) -> Result<Option<String>, CliFailure> {
    if args.all_stuck {
        return Ok(None);
    }
    let pr = args.pr.expect("clap requires --all-stuck or PR");
    let head = actions
        .pr_head_ref(repo_slug, pr)
        .map_err(|error| CliFailure::new(1, format!("Could not resolve PR #{pr}: {error}")))?
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!("PR #{pr} returned no head ref; is it open on {repo_slug}?"),
            )
        })?;
    Ok(Some(head))
}

fn collect_candidates(
    args: &RescueArgs,
    actions: &GitHubActions,
    repo_slug: &str,
    branch: Option<&str>,
    threshold_secs: i64,
    now: DateTime<Utc>,
) -> Result<Vec<Candidate>, CliFailure> {
    let queued = actions
        .list_queued_runs_paginated(repo_slug, RESCUE_LIST_MAX_PAGES)
        .map_err(|error| CliFailure::new(1, format!("Could not list queued runs: {error}")))?;
    let stuck = filter_stuck_queued(&queued, branch, threshold_secs, now)
        .into_iter()
        .filter(rescue_run_cancellation_safe)
        .collect::<Vec<_>>();
    let mut candidates: Vec<Candidate> = stuck
        .into_iter()
        .map(|run| Candidate {
            kind: CandidateKind::QueuedStuck,
            run,
        })
        .collect();
    if !args.rerun_failed {
        return Ok(candidates);
    }
    let completed = actions
        .list_runs_with_status_paginated(repo_slug, "completed", branch, RESCUE_LIST_MAX_PAGES)
        .map_err(|error| CliFailure::new(1, format!("Could not list completed runs: {error}")))?;
    for run in completed {
        if !matches_branch(&run, branch) {
            continue;
        }
        if !rescue_run_cancellation_safe(&run) {
            continue;
        }
        // A watchdog sweep marks a wedged run `cancelled`; a flaky or
        // genuinely-failed required leg ends `failure`/`timed_out`. All three
        // are re-dispatchable under --rerun-failed (#345) — the previous code
        // only caught `cancelled`, so a plain failed macOS leg was never a
        // rescue candidate.
        if run
            .conclusion
            .as_deref()
            .is_some_and(|value| matches!(value, "cancelled" | "failure" | "timed_out"))
        {
            candidates.push(Candidate {
                kind: CandidateKind::CompletedRerunnable,
                run,
            });
        }
    }
    Ok(candidates)
}

fn rescue_run_cancellation_safe(run: &QueuedRun) -> bool {
    is_bulk_run_cancellation_safe(run)
}

fn rescue_envelope_data(
    args: &RescueArgs,
    repo_slug: &str,
    branch: Option<&str>,
    threshold_secs: i64,
    candidates: &[Candidate],
    rows: &[BTreeMap<String, Value>],
) -> BTreeMap<String, Value> {
    let mut data = BTreeMap::new();
    data.insert("event".to_owned(), Value::from("rescue"));
    data.insert("repo".to_owned(), Value::from(repo_slug.to_owned()));
    data.insert("pr".to_owned(), args.pr.map_or(Value::Null, Value::from));
    data.insert(
        "branch".to_owned(),
        branch.map_or(Value::Null, |value| Value::from(value.to_owned())),
    );
    data.insert("all_stuck".to_owned(), Value::Bool(args.all_stuck));
    data.insert(
        "provider".to_owned(),
        args.provider
            .clone()
            .map_or_else(|| Value::from("auto"), Value::from),
    );
    data.insert("rerun_failed".to_owned(), Value::Bool(args.rerun_failed));
    data.insert("dry_run".to_owned(), Value::Bool(args.dry_run));
    data.insert("threshold_secs".to_owned(), Value::from(threshold_secs));
    data.insert("candidate_count".to_owned(), Value::from(candidates.len()));
    data.insert(
        "runs".to_owned(),
        Value::Array(
            rows.iter()
                .map(|row| Value::Object(row.iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
                .collect(),
        ),
    );
    data
}

#[derive(Clone, Debug)]
struct Candidate {
    kind: CandidateKind,
    run: QueuedRun,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CandidateKind {
    QueuedStuck,
    CompletedRerunnable,
}

/// Resolve the provider to redispatch a rescue candidate to.
///
/// An explicit `--to` forces the provider for any candidate. Without it,
/// resolution is kind-aware (#345): a wedged stuck-queued run falls back to
/// `github-hosted` (move off the stuck local runner — the original rescue
/// intent), while a re-run failed/cancelled/timed-out run RE-RESOLVES
/// (`None` → config/default, i.e. local-first with overflow) so a leg that
/// overflowed to a GPU-less hosted runner can return to a real local runner.
fn rescue_provider_override(explicit: Option<&str>, kind: CandidateKind) -> Option<String> {
    match (explicit, kind) {
        (Some(provider), _) => Some(provider.to_owned()),
        (None, CandidateKind::QueuedStuck) => Some("github-hosted".to_owned()),
        (None, CandidateKind::CompletedRerunnable) => None,
    }
}

fn process_candidate(
    candidate: &Candidate,
    args: &RescueArgs,
    workflows: &std::collections::BTreeMap<String, crate::cloud::WorkflowDefinition>,
    config: &LoadedConfig,
    actions: &GitHubActions,
    repo_slug: &str,
) -> RunOutcome {
    if matches!(candidate.kind, CandidateKind::CompletedRerunnable) && !args.rerun_failed {
        return RunOutcome::SkippedCompleted;
    }
    let run = &candidate.run;
    let workflow_file = workflow_filename(&run.path);
    let workflow_key = workflows
        .iter()
        .find(|(_, def)| def.file == workflow_file)
        .map(|(key, _)| key.clone());
    let Some(workflow_key) = workflow_key else {
        return RunOutcome::SkippedNoPlan(format!("no local workflow key matches {workflow_file}"));
    };
    let provider_override = rescue_provider_override(args.provider.as_deref(), candidate.kind);
    let plan = match resolve_cloud_dispatch_plan(
        config,
        workflows,
        &workflow_key,
        &run.head_branch,
        provider_override.as_deref(),
    ) {
        Ok(plan) => plan,
        Err(error) => {
            return RunOutcome::SkippedNoPlan(format!(
                "cannot plan dispatch for {workflow_key}: {error}"
            ));
        }
    };

    match candidate.kind {
        CandidateKind::CompletedRerunnable => {
            if args.dry_run {
                return RunOutcome::Planned;
            }
            if let Err(error) = actions.rerun_failed_run(repo_slug, run.database_id) {
                return RunOutcome::Failed(format!("rerun --failed failed: {error}"));
            }
            if let Err(error) = actions.cancel_workflow_run(repo_slug, run.database_id) {
                return RunOutcome::Failed(format!("cancel failed: {error}"));
            }
            if let Err(error) = actions.workflow_dispatch(
                Some(repo_slug),
                &plan.workflow.file,
                &plan.ref_name,
                &plan.dispatch_fields,
            ) {
                return RunOutcome::Failed(format!("workflow_dispatch failed: {error}"));
            }
            RunOutcome::RerunAndApplied
        }
        CandidateKind::QueuedStuck => {
            if args.dry_run {
                return RunOutcome::Planned;
            }
            if let Err(error) = actions.cancel_workflow_run(repo_slug, run.database_id) {
                return RunOutcome::Failed(format!("cancel failed: {error}"));
            }
            if let Err(error) = actions.workflow_dispatch(
                Some(repo_slug),
                &plan.workflow.file,
                &plan.ref_name,
                &plan.dispatch_fields,
            ) {
                return RunOutcome::Failed(format!("workflow_dispatch failed: {error}"));
            }
            RunOutcome::Applied
        }
    }
}

fn candidate_row(candidate: &Candidate, outcome: &RunOutcome) -> BTreeMap<String, Value> {
    let mut row = BTreeMap::new();
    let run = &candidate.run;
    row.insert("run_id".to_owned(), Value::from(run.database_id));
    row.insert(
        "workflow".to_owned(),
        Value::from(if run.workflow_name.is_empty() {
            run.name.clone()
        } else {
            run.workflow_name.clone()
        }),
    );
    row.insert("branch".to_owned(), Value::from(run.head_branch.clone()));
    row.insert(
        "kind".to_owned(),
        Value::from(match candidate.kind {
            CandidateKind::QueuedStuck => "queued-stuck",
            CandidateKind::CompletedRerunnable => "completed-cancelled",
        }),
    );
    row.insert("status".to_owned(), Value::from(outcome.label()));
    if let Some(detail) = outcome.detail() {
        row.insert("detail".to_owned(), Value::from(detail));
    }
    row.insert(
        "url".to_owned(),
        run.url.clone().map_or(Value::Null, Value::from),
    );
    row
}

fn render_human_summary(
    args: &RescueArgs,
    repo_slug: &str,
    branch: Option<&str>,
    rows: &[BTreeMap<String, Value>],
) -> String {
    let scope = if args.all_stuck {
        format!("repo {repo_slug}")
    } else {
        let pr = args
            .pr
            .map_or_else(|| String::from("?"), |value| value.to_string());
        let branch = branch.unwrap_or("?");
        format!("PR #{pr} ({branch}) on {repo_slug}")
    };

    if rows.is_empty() {
        if args.rerun_failed {
            return format!(
                "No stuck or cancelled runs found for {scope} older than {}.",
                args.threshold
            );
        }
        return format!(
            "No stuck queued runs for {scope} older than {}. Pass --rerun-failed to also scan completed/cancelled runs.",
            args.threshold
        );
    }

    let mut lines = Vec::new();
    let provider_label = args.provider.as_deref().unwrap_or("auto");
    let header = if args.dry_run {
        format!(
            "Rescue plan for {scope} (provider: {provider_label}). Dry-run; re-run without --dry-run to apply."
        )
    } else {
        format!("Rescued {scope} (provider: {provider_label}).")
    };
    lines.push(header);
    for row in rows {
        let run_id = row
            .get("run_id")
            .and_then(Value::as_u64)
            .map_or_else(|| "?".to_owned(), |value| value.to_string());
        let workflow = row.get("workflow").and_then(Value::as_str).unwrap_or("?");
        let status = row.get("status").and_then(Value::as_str).unwrap_or("?");
        let kind = row.get("kind").and_then(Value::as_str).unwrap_or("");
        let detail = row
            .get("detail")
            .and_then(Value::as_str)
            .map(|value| format!(" — {value}"))
            .unwrap_or_default();
        lines.push(format!(
            "  • {workflow} (run {run_id}, {kind}) → {status}{detail}"
        ));
    }
    if !args.rerun_failed {
        lines.push(
            "Hint: pass --rerun-failed to also re-arm completed/cancelled runs (e.g. watchdog-cancelled)."
                .to_owned(),
        );
    }
    lines.join("\n")
}

fn render<W: Write>(
    stdout: &mut W,
    json: bool,
    data: BTreeMap<String, Value>,
    human: impl FnOnce() -> String,
) -> Result<(), CliFailure> {
    if json {
        write_json_envelope(stdout, RESCUE_EVENT, data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    writeln!(stdout, "{}", human()).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn filter_stuck_queued(
    runs: &[QueuedRun],
    branch: Option<&str>,
    threshold_secs: i64,
    now: DateTime<Utc>,
) -> Vec<QueuedRun> {
    runs.iter()
        .filter(|run| matches_branch(run, branch))
        .filter(|run| run_age_secs(run, now).is_some_and(|age| age >= threshold_secs))
        .cloned()
        .collect()
}

fn matches_branch(run: &QueuedRun, branch: Option<&str>) -> bool {
    match branch {
        Some(branch) => run.head_branch == branch,
        None => true,
    }
}

fn run_age_secs(run: &QueuedRun, now: DateTime<Utc>) -> Option<i64> {
    let created = DateTime::parse_from_rfc3339(&run.created_at).ok()?;
    Some((now - created.with_timezone(&Utc)).num_seconds())
}

/// Extract a workflow filename from a workflow-run `path` field.
///
/// GitHub's Actions API returns `path` values like
/// `.github/workflows/ci.yml@refs/heads/main` or
/// `.github/workflows/ci.yml@main`; only the `ci.yml` portion matches the
/// `WorkflowDefinition::file` we discovered locally. The `@<ref>` suffix
/// must be stripped *before* taking the basename — a ref like
/// `refs/heads/main` contains slashes, which `Path::file_name` would
/// otherwise read as nested directories and return `main`. Strip the
/// `@<ref>` first so the basename is computed against the file path alone.
fn workflow_filename(path: &str) -> String {
    let file_segment = path.split_once('@').map_or(path, |(file, _ref)| file);
    Path::new(file_segment)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(file_segment)
        .to_owned()
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

fn resolve_repo_slug(repo: Option<String>, cwd: &Path) -> Result<String, CliFailure> {
    if let Some(repo) = repo.filter(|value| !value.is_empty()) {
        return Ok(repo);
    }
    let output = std::process::Command::new("git")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use chrono::TimeZone;

    fn config(root: &Path) -> LoadedConfig {
        LoadedConfig {
            data: toml::Table::new(),
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn queued_run(
        id: u64,
        branch: &str,
        created_at: &str,
        status: &str,
        conclusion: Option<&str>,
    ) -> QueuedRun {
        QueuedRun {
            database_id: id,
            name: format!("CI #{id}"),
            head_branch: branch.to_owned(),
            event: "pull_request".to_owned(),
            created_at: created_at.to_owned(),
            run_started_at: None,
            workflow_name: "CI".to_owned(),
            url: Some(format!("https://example/run/{id}")),
            path: ".github/workflows/ci.yml".to_owned(),
            status: status.to_owned(),
            conclusion: conclusion.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn workflow_filename_strips_ref_suffix() {
        // GitHub's Actions API returns path with `@<ref>` appended in many
        // common cases. We must strip it or workflow lookup fails.
        assert_eq!(
            workflow_filename(".github/workflows/ci.yml@refs/heads/main"),
            "ci.yml"
        );
        assert_eq!(
            workflow_filename(".github/workflows/release.yml@main"),
            "release.yml"
        );
        assert_eq!(workflow_filename("ci.yml@v0.53.0"), "ci.yml");
        // Bare filenames and paths without @ref still work.
        assert_eq!(workflow_filename(".github/workflows/ci.yml"), "ci.yml");
        assert_eq!(workflow_filename("ci.yml"), "ci.yml");
    }

    #[test]
    fn threshold_parsing_matches_handoff_contract() {
        assert_eq!(parse_threshold_secs("30m"), Some(1_800));
        assert_eq!(parse_threshold_secs("2h"), Some(7_200));
        assert_eq!(parse_threshold_secs("45s"), Some(45));
        assert_eq!(parse_threshold_secs("900"), Some(900));
        assert_eq!(parse_threshold_secs(" "), None);
        assert_eq!(parse_threshold_secs("later"), None);
    }

    #[test]
    fn filter_stuck_queued_respects_branch_and_age() {
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let runs = vec![
            queued_run(1, "feat/x", "2026-05-13T11:50:00Z", "queued", None), // 10m
            queued_run(2, "feat/x", "2026-05-13T10:00:00Z", "queued", None), // 2h
            queued_run(3, "main", "2026-05-13T08:00:00Z", "queued", None),   // 4h, wrong branch
        ];

        let only_x = filter_stuck_queued(&runs, Some("feat/x"), 1_800, now);
        assert_eq!(only_x.len(), 1);
        assert_eq!(only_x[0].database_id, 2);

        let all = filter_stuck_queued(&runs, None, 1_800, now);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn human_summary_explains_dry_run() {
        let args = RescueArgs {
            pr: Some(7),
            all_stuck: false,
            provider: Some("github-hosted".to_owned()),
            rerun_failed: false,
            dry_run: true,
            threshold: "30m".to_owned(),
            repo: Some("owner/repo".to_owned()),
        };
        let run = queued_run(99, "feat/x", "2026-05-13T10:00:00Z", "queued", None);
        let candidate = Candidate {
            kind: CandidateKind::QueuedStuck,
            run,
        };
        let row = candidate_row(&candidate, &RunOutcome::Planned);
        let summary = render_human_summary(&args, "owner/repo", Some("feat/x"), &[row]);
        assert!(summary.contains("Dry-run"));
        assert!(summary.contains("run 99"));
        assert!(summary.contains("planned"));
        assert!(summary.contains("--rerun-failed"));
    }

    #[test]
    fn human_summary_no_op_includes_threshold_hint() {
        let args = RescueArgs {
            pr: Some(7),
            all_stuck: false,
            provider: Some("github-hosted".to_owned()),
            rerun_failed: false,
            dry_run: false,
            threshold: "30m".to_owned(),
            repo: Some("owner/repo".to_owned()),
        };
        let summary = render_human_summary(&args, "owner/repo", Some("feat/x"), &[]);
        assert!(summary.contains("No stuck queued runs"));
        assert!(summary.contains("30m"));
        assert!(summary.contains("--rerun-failed"));
    }

    #[test]
    fn dry_run_planning_uses_local_workflow_match() {
        use std::fs;
        let temp = tempfile::tempdir().expect("tempdir");
        let workflows_dir = temp.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).expect("mkdir");
        fs::write(
            workflows_dir.join("ci.yml"),
            "name: CI\non: workflow_dispatch\njobs: {}\n",
        )
        .expect("write workflow");

        let workflows = discover_workflows(temp.path());
        let cfg = config(temp.path());
        let run = queued_run(42, "feat/x", "2026-05-13T10:00:00Z", "queued", None);
        let candidate = Candidate {
            kind: CandidateKind::QueuedStuck,
            run,
        };
        let args = RescueArgs {
            pr: Some(7),
            all_stuck: false,
            provider: Some("github-hosted".to_owned()),
            rerun_failed: false,
            dry_run: true,
            threshold: "30m".to_owned(),
            repo: Some("owner/repo".to_owned()),
        };
        let actions = GitHubActions::new(temp.path());
        let outcome =
            process_candidate(&candidate, &args, &workflows, &cfg, &actions, "owner/repo");
        assert_eq!(outcome, RunOutcome::Planned);
    }

    #[test]
    fn rescue_provider_override_is_kind_aware_without_explicit_to() {
        // #345: explicit --to forces the provider for any candidate kind.
        assert_eq!(
            rescue_provider_override(Some("local"), CandidateKind::QueuedStuck),
            Some("local".to_owned())
        );
        assert_eq!(
            rescue_provider_override(Some("github-hosted"), CandidateKind::CompletedRerunnable),
            Some("github-hosted".to_owned())
        );
        // Without --to: a wedged stuck-queued run moves OFF the local runner to
        // github-hosted; a re-run failed run RE-RESOLVES (None → local-first
        // with overflow) so an overflowed-to-hosted leg can return local.
        assert_eq!(
            rescue_provider_override(None, CandidateKind::QueuedStuck),
            Some("github-hosted".to_owned())
        );
        assert_eq!(
            rescue_provider_override(None, CandidateKind::CompletedRerunnable),
            None
        );
    }

    #[test]
    fn bulk_rescue_is_review_only_and_release_workflows_are_always_protected() {
        let review = queued_run(1, "feature/x", "2026-05-13T10:00:00Z", "queued", None);
        assert!(rescue_run_cancellation_safe(&review));

        let mut dispatch = review.clone();
        dispatch.event = "workflow_dispatch".to_owned();
        assert!(!rescue_run_cancellation_safe(&dispatch));

        let mut release = review;
        release.workflow_name = "Release CLI".to_owned();
        release.path = ".github/workflows/release-cli.yml".to_owned();
        assert!(!rescue_run_cancellation_safe(&release));
    }

    #[test]
    fn completed_cancelled_skipped_without_rerun_flag() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workflows = discover_workflows(temp.path());
        let cfg = config(temp.path());
        let run = queued_run(
            42,
            "feat/x",
            "2026-05-13T10:00:00Z",
            "completed",
            Some("cancelled"),
        );
        let candidate = Candidate {
            kind: CandidateKind::CompletedRerunnable,
            run,
        };
        let args = RescueArgs {
            pr: Some(7),
            all_stuck: false,
            provider: Some("github-hosted".to_owned()),
            rerun_failed: false,
            dry_run: true,
            threshold: "30m".to_owned(),
            repo: Some("owner/repo".to_owned()),
        };
        let actions = GitHubActions::new(temp.path());
        let outcome =
            process_candidate(&candidate, &args, &workflows, &cfg, &actions, "owner/repo");
        // Skipped because process_candidate is called for cancelled but rerun_failed is off.
        assert_eq!(outcome, RunOutcome::SkippedCompleted);
    }

    #[test]
    fn skips_when_no_local_workflow_matches() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workflows = discover_workflows(temp.path()); // empty
        let cfg = config(temp.path());
        let run = queued_run(42, "feat/x", "2026-05-13T10:00:00Z", "queued", None);
        let candidate = Candidate {
            kind: CandidateKind::QueuedStuck,
            run,
        };
        let args = RescueArgs {
            pr: Some(7),
            all_stuck: false,
            provider: Some("github-hosted".to_owned()),
            rerun_failed: false,
            dry_run: true,
            threshold: "30m".to_owned(),
            repo: Some("owner/repo".to_owned()),
        };
        let actions = GitHubActions::new(temp.path());
        let outcome =
            process_candidate(&candidate, &args, &workflows, &cfg, &actions, "owner/repo");
        assert!(matches!(outcome, RunOutcome::SkippedNoPlan(_)));
    }
}
