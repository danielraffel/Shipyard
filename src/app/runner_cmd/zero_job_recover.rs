//! Bounded recovery for the Pulp Actions "run exists but has zero jobs" wedge.
//!
//! This is deliberately independent from merge-steward coalescing and stale-run
//! cancellation. It never cancels, reruns, queues, labels, pushes, or changes
//! `TartCI` state. Its only apply-mode writes are an at-most-once commit-status
//! receipt followed by a protected-main `build-macos.yml` dispatch.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::ExitCode;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::CliFailure;
use crate::cloud::GitHubActions;
use crate::output::write_json_envelope;

const PULP_REPO: &str = "Generous-Corp/pulp";
const BASE_BRANCH: &str = "main";
const SOURCE_WORKFLOW_ID: u64 = 256_999_733;
const SOURCE_WORKFLOW_NAME: &str = "Build and Test";
const SOURCE_WORKFLOW_PATH: &str = ".github/workflows/build.yml";
const GITHUB_ACTIONS_APP_ID: u64 = 15_368;
const RECOVERY_WORKFLOW_PATH: &str = ".github/workflows/build-macos.yml";
const CONTROLLER_WORKFLOW: &str = "Shipyard merge steward";
const CONTROLLER_WORKFLOW_REF: &str =
    "Generous-Corp/pulp/.github/workflows/shipyard-merge-steward.yml@refs/heads/main";
const CONTROLLER_WORKFLOW_PATH: &str = ".github/workflows/shipyard-merge-steward.yml";
const RECEIPT_CONTEXT: &str = "shipyard/zero-job-redispatch";
const MANAGED_LABEL: &str = "shipyard:managed";
const OPT_OUT_LABEL: &str = "shipyard:unmanaged";
const PROVENANCE_BLOCK_LABEL: &str = "5·unresolved";
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 20;

pub(super) struct ZeroJobRecoverArgs {
    pub(super) repo: String,
    pub(super) pr: u64,
    pub(super) source_run_id: u64,
    pub(super) min_age_minutes: i64,
    pub(super) apply: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Candidate {
    pr: u64,
    head_sha: String,
    head_ref: String,
    source_run_id: u64,
    source_run_attempt: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ControllerAuthority {
    run_id: u64,
    run_attempt: u64,
    head_sha: String,
    event: String,
}

#[derive(Clone, Debug)]
struct Observation {
    pr: Value,
    workflow: Value,
    required_checks: Value,
    source_run: Value,
    source_jobs: Vec<Value>,
    active_same_head_runs: Vec<Value>,
    check_runs: Vec<Value>,
    statuses: Vec<Value>,
    now: DateTime<Utc>,
}

#[derive(Serialize)]
struct Report {
    repo: String,
    pr: u64,
    source_run_id: u64,
    eligible: bool,
    applied: bool,
    head_sha: Option<String>,
    source_run_attempt: Option<u64>,
    action: String,
}

pub(super) fn zero_job_recover_command<W: Write>(
    args: ZeroJobRecoverArgs,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    zero_job_recover_command_with_controller(args, actions, json_output, stdout, None)
}

fn zero_job_recover_command_with_controller<W: Write>(
    args: ZeroJobRecoverArgs,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
    controller_override: Option<ControllerAuthority>,
) -> Result<ExitCode, CliFailure> {
    validate_args(&args)?;
    let scoped = actions.clone().with_repo_override(&args.repo);
    let observation = observe(&scoped, &args)?;
    let candidate = classify(&args, &observation).map_err(|reason| CliFailure::new(3, reason))?;

    if !args.apply {
        return render(
            stdout,
            json_output,
            &Report {
                repo: args.repo,
                pr: args.pr,
                source_run_id: args.source_run_id,
                eligible: true,
                applied: false,
                head_sha: Some(candidate.head_sha),
                source_run_attempt: Some(candidate.source_run_attempt),
                action: "dry-run: exact candidate observed; no GitHub state changed".to_owned(),
            },
        );
    }
    let controller = controller_override.map_or_else(controller_authority_from_env, Ok)?;
    validate_live_controller(&scoped, &args.repo, &controller)?;

    // The receipt is intentionally spent before dispatch. If the dispatch
    // response is lost or the process crashes, this head is never retried.
    post_receipt(&scoped, &args.repo, &candidate, &controller)?;
    let statuses = fetch_all_array_pages(
        &scoped,
        &format!(
            "repos/{}/commits/{}/statuses",
            args.repo, candidate.head_sha
        ),
        None,
    )?;
    verify_exact_receipt(&statuses, &candidate, &controller)?;

    // Full drift re-read immediately before the sole dispatch mutation. The
    // persisted receipt is allowed here, but every other selector fact must
    // remain identical.
    let reread = observe(&scoped, &args)?;
    let reread_candidate = classify_allowing_exact_receipt(&args, &reread, &candidate, &controller)
        .map_err(|reason| CliFailure::new(3, reason))?;
    if reread_candidate != candidate {
        return Err(CliFailure::new(
            3,
            "zero-job candidate drifted before dispatch",
        ));
    }
    validate_live_controller(&scoped, &args.repo, &controller)?;

    let fields = BTreeMap::from([
        ("pr_number".to_owned(), candidate.pr.to_string()),
        ("target_ref".to_owned(), candidate.head_ref.clone()),
        ("expected_head_sha".to_owned(), candidate.head_sha.clone()),
        (
            "source_run_id".to_owned(),
            candidate.source_run_id.to_string(),
        ),
        (
            "source_run_attempt".to_owned(),
            candidate.source_run_attempt.to_string(),
        ),
        ("recovery".to_owned(), "true".to_owned()),
        ("runner".to_owned(), "github-hosted".to_owned()),
    ]);
    scoped
        .workflow_dispatch(
            Some(&args.repo),
            RECOVERY_WORKFLOW_PATH,
            BASE_BRANCH,
            &fields,
        )
        .map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "protected recovery dispatch failed after the at-most-once receipt was spent: {error}"
                ),
            )
        })?;

    render(
        stdout,
        json_output,
        &Report {
            repo: args.repo,
            pr: args.pr,
            source_run_id: args.source_run_id,
            eligible: true,
            applied: true,
            head_sha: Some(candidate.head_sha),
            source_run_attempt: Some(candidate.source_run_attempt),
            action: "receipt persisted; protected build-macos recovery dispatched".to_owned(),
        },
    )
}

fn validate_args(args: &ZeroJobRecoverArgs) -> Result<(), CliFailure> {
    if args.repo != PULP_REPO {
        return Err(CliFailure::new(
            2,
            "zero-job recovery is restricted to Generous-Corp/pulp",
        ));
    }
    if args.pr == 0 || args.source_run_id == 0 || args.min_age_minutes < 45 {
        return Err(CliFailure::new(
            2,
            "--pr and --source-run-id must be positive and --min-age-minutes must be at least 45",
        ));
    }
    Ok(())
}

fn observe(actions: &GitHubActions, args: &ZeroJobRecoverArgs) -> Result<Observation, CliFailure> {
    let pr = get_json(actions, &format!("repos/{}/pulls/{}", args.repo, args.pr))?;
    let workflow = get_json(
        actions,
        &format!("repos/{}/actions/workflows/{SOURCE_WORKFLOW_ID}", args.repo),
    )?;
    let required_checks = get_json(
        actions,
        &format!(
            "repos/{}/branches/{BASE_BRANCH}/protection/required_status_checks",
            args.repo
        ),
    )?;
    let source_run = get_json(
        actions,
        &format!("repos/{}/actions/runs/{}", args.repo, args.source_run_id),
    )?;
    let source_jobs = fetch_jobs(actions, &args.repo, args.source_run_id)?;
    let head_ref = pointer_str(&pr, "/head/ref").map_err(|error| CliFailure::new(1, error))?;
    let active_same_head_runs = fetch_bounded_workflow_runs(
        actions,
        &format!(
            "repos/{}/actions/workflows/{SOURCE_WORKFLOW_ID}/runs",
            args.repo
        ),
        &[("event", "pull_request"), ("branch", head_ref)],
    )?;
    let head_sha = pointer_str(&pr, "/head/sha").map_err(|error| CliFailure::new(1, error))?;
    let check_runs = fetch_check_runs(actions, &args.repo, head_sha)?;
    let statuses = fetch_all_array_pages(
        actions,
        &format!("repos/{}/commits/{head_sha}/statuses", args.repo),
        None,
    )?;
    Ok(Observation {
        pr,
        workflow,
        required_checks,
        source_run,
        source_jobs,
        active_same_head_runs,
        check_runs,
        statuses,
        now: Utc::now(),
    })
}

fn classify(args: &ZeroJobRecoverArgs, observation: &Observation) -> Result<Candidate, String> {
    classify_inner(args, observation, None)
}

fn classify_allowing_exact_receipt(
    args: &ZeroJobRecoverArgs,
    observation: &Observation,
    expected: &Candidate,
    controller: &ControllerAuthority,
) -> Result<Candidate, String> {
    classify_inner(args, observation, Some((expected, controller)))
}

#[allow(clippy::too_many_lines)]
fn classify_inner(
    args: &ZeroJobRecoverArgs,
    o: &Observation,
    allowed_receipt: Option<(&Candidate, &ControllerAuthority)>,
) -> Result<Candidate, String> {
    let state = pointer_str(&o.pr, "/state")?;
    if state != "open" || o.pr.get("draft").and_then(Value::as_bool) != Some(false) {
        return Err("pull request is not open and non-draft".to_owned());
    }
    if pointer_str(&o.pr, "/base/ref")? != BASE_BRANCH {
        return Err("pull request base is not main".to_owned());
    }
    if !pointer_str(&o.pr, "/head/repo/full_name")?.eq_ignore_ascii_case(PULP_REPO) {
        return Err("fork pull requests are ineligible".to_owned());
    }
    let head_sha = pointer_str(&o.pr, "/head/sha")?.to_ascii_lowercase();
    if !is_full_sha(&head_sha) {
        return Err("pull request head is not a full SHA".to_owned());
    }
    let head_ref = pointer_str(&o.pr, "/head/ref")?.to_owned();
    if head_ref.is_empty() {
        return Err("pull request head ref is empty".to_owned());
    }
    let labels =
        o.pr.get("labels")
            .and_then(Value::as_array)
            .ok_or_else(|| "pull request labels are missing".to_owned())?;
    let has_label = |wanted: &str| {
        labels.iter().any(|label| {
            label
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
        })
    };
    if !has_label(MANAGED_LABEL) || has_label(OPT_OUT_LABEL) || has_label(PROVENANCE_BLOCK_LABEL) {
        return Err("pull request is not safely steward-managed".to_owned());
    }
    if !latest_success_status(&o.statuses, "shipyard/steward-handoff") {
        return Err("current head lacks a successful steward handoff status".to_owned());
    }
    if o.workflow.get("id").and_then(Value::as_u64) != Some(SOURCE_WORKFLOW_ID)
        || pointer_str(&o.workflow, "/name")? != SOURCE_WORKFLOW_NAME
        || pointer_str(&o.workflow, "/path")? != SOURCE_WORKFLOW_PATH
    {
        return Err("Build and Test workflow identity drifted".to_owned());
    }
    if !required_macos_actions_check(&o.required_checks) {
        return Err("main does not require the GitHub Actions macos context".to_owned());
    }
    let run = &o.source_run;
    if run.get("id").and_then(Value::as_u64) != Some(args.source_run_id)
        || run.get("workflow_id").and_then(Value::as_u64) != Some(SOURCE_WORKFLOW_ID)
        || pointer_str(run, "/name")? != SOURCE_WORKFLOW_NAME
        || pointer_str(run, "/path")? != SOURCE_WORKFLOW_PATH
        || pointer_str(run, "/event")? != "pull_request"
        || !matches!(pointer_str(run, "/status")?, "queued" | "pending")
        || run.get("conclusion") != Some(&Value::Null)
        || pointer_str(run, "/head_branch")? != head_ref
        || !pointer_str(run, "/head_sha")?.eq_ignore_ascii_case(&head_sha)
    {
        return Err(
            "source run is not the exact queued-or-pending current-head Build and Test run"
                .to_owned(),
        );
    }
    let run_prs = run
        .get("pull_requests")
        .and_then(Value::as_array)
        .ok_or_else(|| "source run pull-request association is missing".to_owned())?;
    if run_prs.len() != 1 || run_prs[0].get("number").and_then(Value::as_u64) != Some(args.pr) {
        return Err("source run is not associated with exactly the requested PR".to_owned());
    }
    let attempt = run
        .get("run_attempt")
        .and_then(Value::as_u64)
        .filter(|attempt| *attempt > 0)
        .ok_or_else(|| "source run attempt is missing or zero".to_owned())?;
    let run_started_at_raw = pointer_str(run, "/run_started_at")?;
    let run_started_at = DateTime::parse_from_rfc3339(run_started_at_raw)
        .map_err(|_| "source run run_started_at is invalid".to_owned())?
        .with_timezone(&Utc);
    if o.now.signed_duration_since(run_started_at) < Duration::minutes(args.min_age_minutes) {
        return Err("source run has not reached the recovery age threshold".to_owned());
    }
    if !o.source_jobs.is_empty() {
        return Err("source run materialized one or more jobs".to_owned());
    }
    if o.active_same_head_runs.iter().any(|candidate| {
        candidate
            .get("head_sha")
            .and_then(Value::as_str)
            .is_none_or(|sha| !is_full_sha(sha))
    }) {
        return Err("workflow census contains a missing or invalid head SHA".to_owned());
    }
    let same_head = o
        .active_same_head_runs
        .iter()
        .filter(|candidate| {
            candidate
                .get("head_sha")
                .and_then(Value::as_str)
                .is_some_and(|sha| sha.eq_ignore_ascii_case(&head_sha))
        })
        .collect::<Vec<_>>();
    if same_head.iter().any(|candidate| {
        candidate.get("workflow_id").and_then(Value::as_u64) != Some(SOURCE_WORKFLOW_ID)
            || candidate.get("name").and_then(Value::as_str) != Some(SOURCE_WORKFLOW_NAME)
            || candidate.get("path").and_then(Value::as_str) != Some(SOURCE_WORKFLOW_PATH)
            || candidate.get("event").and_then(Value::as_str) != Some("pull_request")
            || candidate.get("head_branch").and_then(Value::as_str) != Some(head_ref.as_str())
            || candidate
                .get("head_sha")
                .and_then(Value::as_str)
                .is_none_or(|sha| !sha.eq_ignore_ascii_case(&head_sha))
    }) {
        return Err("an active same-head run has ambiguous workflow identity".to_owned());
    }
    let mut active = Vec::new();
    for candidate in same_head {
        match candidate.get("status").and_then(Value::as_str) {
            Some("completed") => {}
            status if active_status(status) => active.push(candidate),
            _ => {
                return Err("a same-head run has missing or unknown status".to_owned());
            }
        }
    }
    if active.len() != 1
        || active[0].get("id").and_then(Value::as_u64) != Some(args.source_run_id)
        || active[0].get("run_attempt").and_then(Value::as_u64) != Some(attempt)
        || active[0].get("status").and_then(Value::as_str)
            != run.get("status").and_then(Value::as_str)
        || active[0].get("conclusion") != Some(&Value::Null)
        || active[0].get("run_started_at").and_then(Value::as_str) != Some(run_started_at_raw)
    {
        return Err("expected exactly one active same-head Build and Test run".to_owned());
    }
    if o.check_runs
        .iter()
        .any(|check| check.get("name").and_then(Value::as_str) == Some("macos"))
    {
        return Err("a macos check already exists on the current head".to_owned());
    }
    let receipt = unique_status(&o.statuses, RECEIPT_CONTEXT)?;
    match (receipt, allowed_receipt) {
        (None, None) => {}
        (Some(status), Some((expected, controller)))
            if receipt_matches(status, expected, controller) => {}
        (Some(_), _) => {
            return Err("a zero-job redispatch receipt already exists on this head".to_owned());
        }
        (None, Some(_)) => {
            return Err("the spent zero-job receipt disappeared before dispatch".to_owned());
        }
    }
    Ok(Candidate {
        pr: args.pr,
        head_sha,
        head_ref,
        source_run_id: args.source_run_id,
        source_run_attempt: attempt,
    })
}

fn required_macos_actions_check(value: &Value) -> bool {
    value
        .get("checks")
        .and_then(Value::as_array)
        .is_some_and(|checks| {
            checks.iter().any(|check| {
                check.get("context").and_then(Value::as_str) == Some("macos")
                    && check.get("app_id").and_then(Value::as_u64) == Some(GITHUB_ACTIONS_APP_ID)
            })
        })
}

fn active_status(status: Option<&str>) -> bool {
    status.is_some_and(|status| {
        matches!(
            status.to_ascii_lowercase().as_str(),
            "pending" | "queued" | "requested" | "waiting" | "in_progress"
        )
    })
}

fn post_receipt(
    actions: &GitHubActions,
    repo: &str,
    candidate: &Candidate,
    controller: &ControllerAuthority,
) -> Result<(), CliFailure> {
    let description = receipt_description(candidate, controller);
    let args = vec![
        "api".to_owned(),
        "--method".to_owned(),
        "POST".to_owned(),
        format!("repos/{repo}/statuses/{}", candidate.head_sha),
        "-f".to_owned(),
        "state=success".to_owned(),
        "-f".to_owned(),
        format!("context={RECEIPT_CONTEXT}"),
        "-f".to_owned(),
        format!("description={description}"),
        "-f".to_owned(),
        format!(
            "target_url=https://github.com/{repo}/actions/runs/{}",
            candidate.source_run_id
        ),
    ];
    actions
        .run_gh(&args)
        .map(|_| ())
        .map_err(|error| CliFailure::new(1, format!("could not persist zero-job receipt: {error}")))
}

fn verify_exact_receipt(
    statuses: &[Value],
    candidate: &Candidate,
    controller: &ControllerAuthority,
) -> Result<(), CliFailure> {
    let status = unique_status(statuses, RECEIPT_CONTEXT)
        .map_err(|error| CliFailure::new(1, error))?
        .ok_or_else(|| CliFailure::new(1, "zero-job receipt write was not observable"))?;
    if !receipt_matches(status, candidate, controller) {
        return Err(CliFailure::new(
            1,
            "persisted zero-job receipt did not match the candidate",
        ));
    }
    Ok(())
}

fn receipt_matches(
    status: &Value,
    candidate: &Candidate,
    controller: &ControllerAuthority,
) -> bool {
    status.get("state").and_then(Value::as_str) == Some("success")
        && status.get("description").and_then(Value::as_str)
            == Some(receipt_description(candidate, controller).as_str())
        && status.get("target_url").and_then(Value::as_str)
            == Some(
                format!(
                    "https://github.com/{PULP_REPO}/actions/runs/{}",
                    candidate.source_run_id
                )
                .as_str(),
            )
}

fn receipt_description(candidate: &Candidate, controller: &ControllerAuthority) -> String {
    format!(
        "controller={}:{} source={}:{} candidate={}",
        controller.run_id,
        controller.run_attempt,
        candidate.source_run_id,
        candidate.source_run_attempt,
        candidate_fingerprint(candidate)
    )
}

fn candidate_fingerprint(candidate: &Candidate) -> String {
    let material = format!(
        "v1\nrepo={PULP_REPO}\npr={}\nhead={}\nref={}\nsource_run={}\nsource_attempt={}\nworkflow_id={SOURCE_WORKFLOW_ID}\n",
        candidate.pr,
        candidate.head_sha,
        candidate.head_ref,
        candidate.source_run_id,
        candidate.source_run_attempt,
    );
    format!("{:x}", Sha256::digest(material.as_bytes()))
}

fn controller_authority_from_env() -> Result<ControllerAuthority, CliFailure> {
    let exact =
        |name: &str, expected: &str| std::env::var(name).is_ok_and(|value| value == expected);
    if !exact("GITHUB_ACTIONS", "true")
        || !exact("GITHUB_REPOSITORY", PULP_REPO)
        || !exact("GITHUB_WORKFLOW", CONTROLLER_WORKFLOW)
        || !exact("GITHUB_WORKFLOW_REF", CONTROLLER_WORKFLOW_REF)
        || !exact("GITHUB_REF", "refs/heads/main")
    {
        return Err(CliFailure::new(
            2,
            "apply requires the serialized protected-main Pulp Shipyard merge steward workflow",
        ));
    }
    let event = std::env::var("GITHUB_EVENT_NAME").unwrap_or_default();
    if !matches!(event.as_str(), "schedule" | "workflow_dispatch") {
        return Err(CliFailure::new(2, "controller event is not authorized"));
    }
    let head_sha = std::env::var("GITHUB_SHA")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !is_full_sha(&head_sha) {
        return Err(CliFailure::new(2, "GITHUB_SHA must be a full commit SHA"));
    }
    let positive = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| CliFailure::new(2, format!("{name} must be a positive integer")))
    };
    Ok(ControllerAuthority {
        run_id: positive("GITHUB_RUN_ID")?,
        run_attempt: positive("GITHUB_RUN_ATTEMPT")?,
        head_sha,
        event,
    })
}

fn validate_live_controller(
    actions: &GitHubActions,
    repo: &str,
    controller: &ControllerAuthority,
) -> Result<(), CliFailure> {
    let run = get_json(
        actions,
        &format!("repos/{repo}/actions/runs/{}", controller.run_id),
    )?;
    let workflow_id = run
        .get("workflow_id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| CliFailure::new(1, "controller run omitted its workflow identity"))?;
    let workflow = get_json(
        actions,
        &format!("repos/{repo}/actions/workflows/{workflow_id}"),
    )?;
    let observed_workflow_id = workflow
        .get("id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| CliFailure::new(1, "controller workflow omitted its numeric identity"))?;
    if observed_workflow_id != workflow_id
        || workflow.get("name").and_then(Value::as_str) != Some(CONTROLLER_WORKFLOW)
        || workflow.get("path").and_then(Value::as_str) != Some(CONTROLLER_WORKFLOW_PATH)
    {
        return Err(CliFailure::new(3, "controller workflow identity drifted"));
    }
    if run.get("id").and_then(Value::as_u64) != Some(controller.run_id)
        || run.get("workflow_id").and_then(Value::as_u64) != Some(workflow_id)
        || run.get("run_attempt").and_then(Value::as_u64) != Some(controller.run_attempt)
        || run.get("name").and_then(Value::as_str) != Some(CONTROLLER_WORKFLOW)
        || run.get("path").and_then(Value::as_str) != Some(CONTROLLER_WORKFLOW_PATH)
        || run.get("event").and_then(Value::as_str) != Some(controller.event.as_str())
        || run.get("head_branch").and_then(Value::as_str) != Some(BASE_BRANCH)
        || run
            .get("head_sha")
            .and_then(Value::as_str)
            .is_none_or(|sha| !sha.eq_ignore_ascii_case(&controller.head_sha))
        || run.get("status").and_then(Value::as_str) != Some("in_progress")
    {
        return Err(CliFailure::new(
            3,
            "controller run is not the exact active protected-main steward run",
        ));
    }
    Ok(())
}

fn latest_success_status(statuses: &[Value], context: &str) -> bool {
    latest_status(statuses, context).and_then(|status| status.get("state").and_then(Value::as_str))
        == Some("success")
}

fn latest_status<'a>(statuses: &'a [Value], context: &str) -> Option<&'a Value> {
    let mut matching = statuses
        .iter()
        .filter(|status| {
            status
                .get("context")
                .and_then(Value::as_str)
                .is_some_and(|observed| observed.eq_ignore_ascii_case(context))
        })
        .collect::<Vec<_>>();
    matching.sort_by_key(|status| status_order(status));
    matching.pop()
}

fn unique_status<'a>(statuses: &'a [Value], context: &str) -> Result<Option<&'a Value>, String> {
    let mut matching = statuses.iter().filter(|status| {
        status
            .get("context")
            .and_then(Value::as_str)
            .is_some_and(|observed| observed.eq_ignore_ascii_case(context))
    });
    let first = matching.next();
    if matching.next().is_some() {
        return Err(format!(
            "multiple {context} statuses make recovery ownership ambiguous"
        ));
    }
    Ok(first)
}

fn status_order(status: &Value) -> (String, u64) {
    (
        status
            .get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        status.get("id").and_then(Value::as_u64).unwrap_or_default(),
    )
}

fn fetch_jobs(actions: &GitHubActions, repo: &str, run_id: u64) -> Result<Vec<Value>, CliFailure> {
    let endpoint = format!("repos/{repo}/actions/runs/{run_id}/jobs");
    let first = get_json_with_query(
        actions,
        &endpoint,
        &[("filter", "all"), ("per_page", "100"), ("page", "1")],
    )?;
    parse_zero_jobs_response(&first)
}

fn parse_zero_jobs_response(first: &Value) -> Result<Vec<Value>, CliFailure> {
    let total = first
        .get("total_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| CliFailure::new(1, "jobs response omitted total_count"))?;
    let jobs = first
        .get("jobs")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| CliFailure::new(1, "jobs response omitted jobs array"))?;
    if total != jobs.len() as u64 {
        return Err(CliFailure::new(
            1,
            "jobs response count and exhaustive filter=all array disagree",
        ));
    }
    Ok(jobs)
}

fn fetch_check_runs(
    actions: &GitHubActions,
    repo: &str,
    sha: &str,
) -> Result<Vec<Value>, CliFailure> {
    let endpoint = format!("repos/{repo}/commits/{sha}/check-runs");
    let value = get_json_with_query(
        actions,
        &endpoint,
        &[("filter", "all"), ("per_page", "100")],
    )?;
    parse_bounded_check_runs_response(&value)
}

fn fetch_bounded_workflow_runs(
    actions: &GitHubActions,
    endpoint: &str,
    query: &[(&str, &str)],
) -> Result<Vec<Value>, CliFailure> {
    let mut params = query.to_vec();
    params.push(("per_page", "100"));
    params.push(("page", "1"));
    let value = get_json_with_query(actions, endpoint, &params)?;
    parse_bounded_workflow_runs_response(&value)
}

fn parse_bounded_workflow_runs_response(value: &Value) -> Result<Vec<Value>, CliFailure> {
    let total = value
        .get("total_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| CliFailure::new(1, "workflow-runs response omitted total_count"))?;
    let runs = value
        .get("workflow_runs")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| CliFailure::new(1, "workflow-runs response omitted workflow_runs"))?;
    if total > PAGE_SIZE as u64 {
        return Err(CliFailure::new(
            1,
            "workflow-run observation exceeded the bounded census",
        ));
    }
    if total != runs.len() as u64 {
        return Err(CliFailure::new(
            1,
            "workflow-run response count and bounded array disagree",
        ));
    }
    Ok(runs)
}

fn parse_bounded_check_runs_response(value: &Value) -> Result<Vec<Value>, CliFailure> {
    let total = value
        .get("total_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| CliFailure::new(1, "check-runs response omitted total_count"))?;
    let runs = value
        .get("check_runs")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| CliFailure::new(1, "check-runs response omitted check_runs"))?;
    if total > PAGE_SIZE as u64 {
        return Err(CliFailure::new(1, "check-run observation was truncated"));
    }
    if total != runs.len() as u64 {
        return Err(CliFailure::new(
            1,
            "check-run response count and bounded array disagree",
        ));
    }
    Ok(runs)
}

fn fetch_all_array_pages(
    actions: &GitHubActions,
    endpoint: &str,
    query: Option<&[(&str, &str)]>,
) -> Result<Vec<Value>, CliFailure> {
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let mut params = query.unwrap_or_default().to_vec();
        let page_string = page.to_string();
        params.push(("per_page", "100"));
        params.push(("page", &page_string));
        let value = get_json_with_query(actions, endpoint, &params)?;
        let items = if let Some(array) = value.as_array() {
            array
        } else if let Some(array) = value.get("workflow_runs").and_then(Value::as_array) {
            array
        } else {
            return Err(CliFailure::new(
                1,
                "paginated GitHub response omitted its array",
            ));
        };
        all.extend(items.iter().cloned());
        if items.len() < PAGE_SIZE {
            return Ok(all);
        }
    }
    Err(CliFailure::new(
        1,
        "GitHub observation exceeded the exhaustive pagination bound",
    ))
}

fn get_json(actions: &GitHubActions, endpoint: &str) -> Result<Value, CliFailure> {
    get_json_with_query(actions, endpoint, &[])
}

fn get_json_with_query(
    actions: &GitHubActions,
    endpoint: &str,
    query: &[(&str, &str)],
) -> Result<Value, CliFailure> {
    // `gh api -f` otherwise changes the request to POST. All observations are
    // explicitly GET so apply=false is mechanically read-only.
    let mut args = vec![
        "api".to_owned(),
        "--method".to_owned(),
        "GET".to_owned(),
        endpoint.to_owned(),
    ];
    for (key, value) in query {
        args.extend(["-f".to_owned(), format!("{key}={value}")]);
    }
    let raw = actions
        .run_gh(&args)
        .map_err(|error| CliFailure::new(1, format!("GitHub observation failed: {error}")))?;
    serde_json::from_str(&raw)
        .map_err(|error| CliFailure::new(1, format!("GitHub returned invalid JSON: {error}")))
}

fn pointer_str<'a>(value: &'a Value, pointer: &str) -> Result<&'a str, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("GitHub observation omitted {pointer}"))
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn render<W: Write>(
    stdout: &mut W,
    json_output: bool,
    report: &Report,
) -> Result<ExitCode, CliFailure> {
    if json_output {
        let value =
            serde_json::to_value(report).map_err(|error| CliFailure::new(1, error.to_string()))?;
        let data = value
            .as_object()
            .expect("serialized report is an object")
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        write_json_envelope(stdout, "runner.zero-job-recover", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "{}", report.action)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (ZeroJobRecoverArgs, Observation) {
        let head = "a".repeat(40);
        let args = ZeroJobRecoverArgs {
            repo: PULP_REPO.to_owned(),
            pr: 7882,
            source_run_id: 33_438_138_142,
            min_age_minutes: 45,
            apply: false,
        };
        let now = Utc::now();
        let pr = json!({
            "state":"open", "draft":false,
            "base":{"ref":"main"},
            "head":{"sha":head,"ref":"fix/pr-7882","repo":{"full_name":PULP_REPO}},
            "labels":[{"name":MANAGED_LABEL}]
        });
        let workflow = json!({"id":SOURCE_WORKFLOW_ID,"name":SOURCE_WORKFLOW_NAME,"path":SOURCE_WORKFLOW_PATH});
        let required_checks = json!({"checks":[{"context":"macos","app_id":15368}]});
        let source_run = json!({
            "id":args.source_run_id,"workflow_id":SOURCE_WORKFLOW_ID,"name":SOURCE_WORKFLOW_NAME,
            "path":SOURCE_WORKFLOW_PATH,"event":"pull_request","status":"queued","conclusion":null,
            "head_branch":"fix/pr-7882","head_sha":"a".repeat(40),"run_attempt":2,
            "pull_requests":[{"number":args.pr}],
            "created_at":(now-Duration::hours(2)).to_rfc3339(),
            "run_started_at":(now-Duration::minutes(46)).to_rfc3339()
        });
        let handoff = json!({"id":1,"context":"shipyard/steward-handoff","state":"success","updated_at":"2026-08-31T00:00:00Z"});
        (
            args,
            Observation {
                pr,
                workflow,
                required_checks,
                source_run: source_run.clone(),
                source_jobs: Vec::new(),
                active_same_head_runs: vec![source_run],
                check_runs: Vec::new(),
                statuses: vec![handoff],
                now,
            },
        )
    }

    #[test]
    fn exact_zero_job_candidate_is_eligible() {
        let (args, observation) = fixture();
        let candidate = classify(&args, &observation).expect("eligible");
        assert_eq!(candidate.source_run_attempt, 2);
    }

    #[test]
    fn every_material_safety_drift_fails_closed() {
        let mutations: Vec<fn(&mut Observation)> = vec![
            |o| o.pr["state"] = json!("closed"),
            |o| o.pr["draft"] = json!(true),
            |o| o.pr["base"]["ref"] = json!("develop"),
            |o| o.pr["head"]["repo"]["full_name"] = json!("fork/pulp"),
            |o| o.pr["head"]["sha"] = json!("short"),
            |o| o.pr["labels"] = json!([]),
            |o| o.pr["labels"] = json!([{"name":MANAGED_LABEL},{"name":OPT_OUT_LABEL}]),
            |o| o.pr["labels"] = json!([{"name":MANAGED_LABEL},{"name":PROVENANCE_BLOCK_LABEL}]),
            |o| o.workflow["id"] = json!(1),
            |o| o.workflow["name"] = json!("Other"),
            |o| o.workflow["path"] = json!(".github/workflows/other.yml"),
            |o| o.required_checks = json!({"checks":[]}),
            |o| o.source_run["event"] = json!("workflow_dispatch"),
            |o| o.source_run["status"] = json!("in_progress"),
            |o| {
                o.source_run
                    .as_object_mut()
                    .expect("source run object")
                    .remove("conclusion");
            },
            |o| o.source_run["head_sha"] = json!("b".repeat(40)),
            |o| o.source_run["run_attempt"] = json!(0),
            |o| o.source_run["pull_requests"] = json!([]),
            |o| o.source_jobs.push(json!({"id":1})),
            |o| o.active_same_head_runs.push(o.source_run.clone()),
            |o| o.check_runs.push(json!({"name":"macos","status":"queued"})),
            |o| {
                o.statuses.push(json!({"id":2,"context":RECEIPT_CONTEXT,"state":"success","updated_at":"2026-08-31T01:00:00Z"}));
            },
        ];
        for mutate in mutations {
            let (args, mut observation) = fixture();
            mutate(&mut observation);
            assert!(classify(&args, &observation).is_err());
        }
    }

    #[test]
    fn stale_and_missing_handoff_fail_closed() {
        let (args, mut observation) = fixture();
        let fresh = (observation.now - Duration::minutes(2)).to_rfc3339();
        observation.source_run["run_started_at"] = json!(fresh.clone());
        observation.active_same_head_runs[0]["run_started_at"] = json!(fresh);
        assert!(classify(&args, &observation).is_err());
        let (args, mut observation) = fixture();
        observation.statuses.clear();
        assert!(classify(&args, &observation).is_err());
    }

    #[test]
    fn current_attempt_age_requires_exact_valid_run_started_at() {
        for invalid in [None, Some(json!(7)), Some(json!("not-a-time"))] {
            let (args, mut observation) = fixture();
            match invalid {
                Some(value) => observation.source_run["run_started_at"] = value,
                None => {
                    observation
                        .source_run
                        .as_object_mut()
                        .expect("source run object")
                        .remove("run_started_at");
                }
            }
            assert!(classify(&args, &observation).is_err());
        }

        let (args, mut observation) = fixture();
        observation.source_run["created_at"] =
            json!((observation.now - Duration::days(3)).to_rfc3339());
        let fresh = (observation.now - Duration::minutes(2)).to_rfc3339();
        observation.source_run["run_started_at"] = json!(fresh.clone());
        observation.active_same_head_runs[0]["run_started_at"] = json!(fresh);
        assert!(classify(&args, &observation).is_err());
    }

    #[test]
    fn exact_pending_rest_state_is_also_eligible() {
        let (args, mut observation) = fixture();
        observation.source_run["status"] = json!("pending");
        observation.active_same_head_runs[0]["status"] = json!("pending");
        assert!(classify(&args, &observation).is_ok());
    }

    #[test]
    fn extra_same_head_run_with_missing_or_unknown_status_fails_closed() {
        for status in [None, Some("mystery")] {
            let (args, mut observation) = fixture();
            let mut extra = observation.source_run.clone();
            extra["id"] = json!(99);
            match status {
                Some(value) => extra["status"] = json!(value),
                None => {
                    extra.as_object_mut().expect("run object").remove("status");
                }
            }
            observation.active_same_head_runs.push(extra);
            assert!(classify(&args, &observation).is_err());
        }
    }

    #[test]
    fn workflow_census_rejects_missing_non_string_or_invalid_head_sha() {
        for head_sha in [Value::Null, json!(7), json!("short")] {
            let (args, mut observation) = fixture();
            let mut extra = observation.source_run.clone();
            extra["id"] = json!(99);
            extra["head_sha"] = head_sha;
            observation.active_same_head_runs.push(extra);
            assert!(classify(&args, &observation).is_err());
        }
    }

    #[test]
    fn workflow_census_must_match_exact_source_snapshot() {
        let (args, mut observation) = fixture();
        observation.active_same_head_runs[0]["status"] = json!("in_progress");
        assert!(classify(&args, &observation).is_err());

        let (args, mut observation) = fixture();
        observation.active_same_head_runs[0]["run_attempt"] = json!(3);
        assert!(classify(&args, &observation).is_err());

        for census_value in [None, Some(json!(7)), Some(json!("2026-08-31T00:00:00Z"))] {
            let (args, mut observation) = fixture();
            match census_value {
                Some(value) => observation.active_same_head_runs[0]["run_started_at"] = value,
                None => {
                    observation.active_same_head_runs[0]
                        .as_object_mut()
                        .expect("run object")
                        .remove("run_started_at");
                }
            }
            assert!(classify(&args, &observation).is_err());
        }

        let (args, mut observation) = fixture();
        observation.active_same_head_runs[0]
            .as_object_mut()
            .expect("run object")
            .remove("conclusion");
        assert!(classify(&args, &observation).is_err());
    }

    #[test]
    fn exact_spent_receipt_is_only_allowed_for_final_reread() {
        let (args, mut observation) = fixture();
        let candidate = classify(&args, &observation).expect("candidate");
        let controller = ControllerAuthority {
            run_id: 9,
            run_attempt: 2,
            head_sha: "c".repeat(40),
            event: "workflow_dispatch".to_owned(),
        };
        observation.statuses.push(json!({
            "id":2,"context":RECEIPT_CONTEXT,"state":"success",
            "description":receipt_description(&candidate, &controller),
            "target_url":format!("https://github.com/{PULP_REPO}/actions/runs/{}", candidate.source_run_id),
            "updated_at":"2026-08-31T01:00:00Z"
        }));
        assert!(classify(&args, &observation).is_err());
        assert_eq!(
            classify_allowing_exact_receipt(&args, &observation, &candidate, &controller),
            Ok(candidate)
        );
    }

    #[test]
    fn unknown_receipt_and_existing_macos_recovery_are_rejected() {
        let (args, mut observation) = fixture();
        observation.statuses.push(json!({
            "id":2,"context":RECEIPT_CONTEXT,"state":"success",
            "description":"some other run", "target_url":"https://example.invalid",
            "updated_at":"2026-08-31T01:00:00Z"
        }));
        assert!(classify(&args, &observation).is_err());
        let (args, mut observation) = fixture();
        observation
            .check_runs
            .push(json!({"name":"macos","status":"in_progress"}));
        assert!(classify(&args, &observation).is_err());
    }

    #[test]
    fn duplicate_or_conflicting_recovery_receipts_fail_closed() {
        let (args, mut observation) = fixture();
        let candidate = classify(&args, &observation).expect("candidate");
        let controller = ControllerAuthority {
            run_id: 9,
            run_attempt: 2,
            head_sha: "c".repeat(40),
            event: "workflow_dispatch".to_owned(),
        };
        let exact = json!({
            "id":2,"context":RECEIPT_CONTEXT,"state":"success",
            "description":receipt_description(&candidate, &controller),
            "target_url":format!("https://github.com/{PULP_REPO}/actions/runs/{}", candidate.source_run_id),
            "updated_at":"2026-08-31T01:00:00Z"
        });
        observation.statuses.push(exact.clone());
        observation.statuses.push(exact);
        assert!(verify_exact_receipt(&observation.statuses, &candidate, &controller).is_err());
        assert!(
            classify_allowing_exact_receipt(&args, &observation, &candidate, &controller).is_err()
        );

        observation.statuses.pop();
        observation.statuses.push(json!({
            "id":3,"context":RECEIPT_CONTEXT,"state":"success",
            "description":"conflicting receipt",
            "target_url":"https://example.invalid",
            "updated_at":"2026-08-31T02:00:00Z"
        }));
        assert!(verify_exact_receipt(&observation.statuses, &candidate, &controller).is_err());
        assert!(
            classify_allowing_exact_receipt(&args, &observation, &candidate, &controller).is_err()
        );
    }

    #[test]
    fn recovery_receipt_context_is_case_insensitive() {
        let (args, mut observation) = fixture();
        observation.statuses.push(json!({
            "id":2,"context":"Shipyard/Zero-Job-Redispatch","state":"success",
            "description":"some other run", "target_url":"https://example.invalid",
            "updated_at":"2026-08-31T01:00:00Z"
        }));
        assert!(classify(&args, &observation).is_err());

        observation.statuses.push(json!({
            "id":3,"context":RECEIPT_CONTEXT,"state":"success",
            "description":"another run", "target_url":"https://example.invalid/other",
            "updated_at":"2026-08-31T02:00:00Z"
        }));
        assert!(unique_status(&observation.statuses, RECEIPT_CONTEXT).is_err());
    }

    #[test]
    fn receipt_requires_the_exact_canonical_target_url() {
        let (args, _) = fixture();
        let (_, observation) = fixture();
        let candidate = classify(&args, &observation).expect("candidate");
        let controller = ControllerAuthority {
            run_id: 9,
            run_attempt: 2,
            head_sha: "c".repeat(40),
            event: "workflow_dispatch".to_owned(),
        };
        let status = json!({
            "context":RECEIPT_CONTEXT,"state":"success",
            "description":receipt_description(&candidate, &controller),
            "target_url":format!("https://attacker.invalid/actions/runs/{}", candidate.source_run_id)
        });
        assert!(!receipt_matches(&status, &candidate, &controller));
    }

    #[test]
    fn argument_scope_cannot_be_weakened() {
        let (mut args, _) = fixture();
        args.repo = "other/pulp".to_owned();
        assert!(validate_args(&args).is_err());
        args.repo = "generous-corp/pulp".to_owned();
        assert!(validate_args(&args).is_err());
        args.repo = PULP_REPO.to_owned();
        args.min_age_minutes = 44;
        assert!(validate_args(&args).is_err());
    }

    #[test]
    fn zero_job_response_requires_consistent_exhaustive_count() {
        assert!(parse_zero_jobs_response(&json!({"total_count":0,"jobs":[]})).is_ok());
        assert!(parse_zero_jobs_response(&json!({"total_count":0,"jobs":[{"id":1}]})).is_err());
        assert!(parse_zero_jobs_response(&json!({"total_count":1,"jobs":[]})).is_err());
        assert!(parse_zero_jobs_response(&json!({"jobs":[]})).is_err());
    }

    #[test]
    fn check_run_observation_refuses_truncation_and_malformed_counts() {
        assert!(
            parse_bounded_check_runs_response(&json!({"total_count":0,"check_runs":[]})).is_ok()
        );
        assert!(
            parse_bounded_check_runs_response(&json!({"total_count":101,"check_runs":[]})).is_err()
        );
        assert!(
            parse_bounded_check_runs_response(&json!({"total_count":2,"check_runs":[{"id":1}]}))
                .is_err()
        );
        assert!(
            parse_bounded_check_runs_response(&json!({"total_count":0,"check_runs":[{"id":1}]}))
                .is_err()
        );
        assert!(parse_bounded_check_runs_response(&json!({"check_runs":[]})).is_err());
    }

    #[test]
    fn workflow_run_census_refuses_truncation_and_count_drift() {
        assert!(
            parse_bounded_workflow_runs_response(
                &json!({"total_count":1,"workflow_runs":[{"id":1}]})
            )
            .is_ok()
        );
        assert!(
            parse_bounded_workflow_runs_response(
                &json!({"total_count":2,"workflow_runs":[{"id":1}]})
            )
            .is_err()
        );
        assert!(
            parse_bounded_workflow_runs_response(&json!({"total_count":101,"workflow_runs":[]}))
                .is_err()
        );
        assert!(parse_bounded_workflow_runs_response(&json!({"workflow_runs":[]})).is_err());
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn apply_spends_remote_receipt_then_dispatches_once_without_cancellation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let gh = temp.path().join("gh");
        let log = temp.path().join("calls.log");
        let head = "a".repeat(40);
        let source_run_id = 33_438_138_142_u64;
        let controller = ControllerAuthority {
            run_id: 99,
            run_attempt: 3,
            head_sha: "c".repeat(40),
            event: "workflow_dispatch".to_owned(),
        };
        let expected = Candidate {
            pr: 7882,
            head_sha: head.clone(),
            head_ref: "fix/pr-7882".to_owned(),
            source_run_id,
            source_run_attempt: 1,
        };
        let expected_description = receipt_description(&expected, &controller);
        let script = format!(
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"pulls/7882")
    printf '%s' '{{"state":"open","draft":false,"base":{{"ref":"main"}},"head":{{"sha":"{head}","ref":"fix/pr-7882","repo":{{"full_name":"Generous-Corp/pulp"}}}},"labels":[{{"name":"shipyard:managed"}}]}}' ;;
  *"actions/workflows/256999733/runs"*)
    printf '%s' '{{"total_count":1,"workflow_runs":[{{"id":{source_run_id},"workflow_id":256999733,"name":"Build and Test","path":".github/workflows/build.yml","event":"pull_request","head_branch":"fix/pr-7882","head_sha":"{head}","status":"queued","conclusion":null,"run_attempt":1,"run_started_at":"2020-01-01T00:00:00Z"}}]}}' ;;
  *"actions/workflows/256999733")
    printf '%s' '{{"id":256999733,"name":"Build and Test","path":".github/workflows/build.yml"}}' ;;
  *"actions/workflows/777")
    printf '%s' '{{"id":777,"name":"Shipyard merge steward","path":".github/workflows/shipyard-merge-steward.yml"}}' ;;
  *"branches/main/protection/required_status_checks")
    printf '%s' '{{"checks":[{{"context":"macos","app_id":15368}}]}}' ;;
  *"actions/runs/{source_run_id}/jobs"*)
    printf '%s' '{{"total_count":0,"jobs":[]}}' ;;
  *"actions/runs/{source_run_id}")
    printf '%s' '{{"id":{source_run_id},"workflow_id":256999733,"name":"Build and Test","path":".github/workflows/build.yml","event":"pull_request","status":"queued","conclusion":null,"head_branch":"fix/pr-7882","head_sha":"{head}","run_attempt":1,"created_at":"2020-01-01T00:00:00Z","run_started_at":"2020-01-01T00:00:00Z","pull_requests":[{{"number":7882}}]}}' ;;
  *"actions/runs/99")
    printf '%s' '{{"id":99,"workflow_id":777,"name":"Shipyard merge steward","path":".github/workflows/shipyard-merge-steward.yml","event":"workflow_dispatch","status":"in_progress","head_branch":"main","head_sha":"cccccccccccccccccccccccccccccccccccccccc","run_attempt":3}}' ;;
  *"commits/{head}/check-runs"*)
    printf '%s' '{{"total_count":0,"check_runs":[]}}' ;;
  *"commits/{head}/statuses"*)
    if grep -q -- '--method POST' '{log}'; then
      printf '%s' '[{{"id":2,"context":"shipyard/zero-job-redispatch","state":"success","description":"{expected_description}","target_url":"https://github.com/Generous-Corp/pulp/actions/runs/{source_run_id}","updated_at":"2026-08-31T01:00:00Z"}},{{"id":1,"context":"shipyard/steward-handoff","state":"success","updated_at":"2026-08-31T00:00:00Z"}}]'
    else
      printf '%s' '[{{"id":1,"context":"shipyard/steward-handoff","state":"success","updated_at":"2026-08-31T00:00:00Z"}}]'
    fi ;;
  *"--method POST repos/Generous-Corp/pulp/statuses/{head}"*)
    printf '%s' '{{}}' ;;
  "workflow run .github/workflows/build-macos.yml"*)
    grep -q -- '--method POST' '{log}'
    printf '%s' '{{}}' ;;
  *) echo "unexpected gh args: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
        );
        std::fs::write(&gh, script).expect("fake gh");
        let mut permissions = std::fs::metadata(&gh).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&gh, permissions).expect("chmod");
        let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(&gh);
        let mut output = Vec::new();
        let args = ZeroJobRecoverArgs {
            repo: PULP_REPO.to_owned(),
            pr: 7882,
            source_run_id,
            min_age_minutes: 45,
            apply: true,
        };
        let applied = zero_job_recover_command_with_controller(
            args,
            &actions,
            false,
            &mut output,
            Some(controller.clone()),
        );
        if let Err(error) = &applied {
            panic!(
                "apply failed: {error:?}; calls={}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }
        assert_eq!(applied.expect("checked"), ExitCode::SUCCESS);
        let calls = std::fs::read_to_string(&log).expect("calls");
        let receipt_position = calls.find("--method POST").expect("receipt write");
        let dispatch_position = calls.find("workflow run").expect("dispatch");
        assert!(receipt_position < dispatch_position);
        assert!(!calls.contains("cancel"));
        assert_eq!(calls.matches("workflow run").count(), 1);

        let second = ZeroJobRecoverArgs {
            repo: PULP_REPO.to_owned(),
            pr: 7882,
            source_run_id,
            min_age_minutes: 45,
            apply: true,
        };
        assert!(
            zero_job_recover_command_with_controller(
                second,
                &actions,
                false,
                &mut Vec::new(),
                Some(controller),
            )
            .is_err()
        );
        let calls = std::fs::read_to_string(&log).expect("calls after retry");
        assert_eq!(calls.matches("workflow run").count(), 1);
    }
}
