//! `shipyard runner carrier`: observe GitHub, plan each pull request with
//! [`crate::merge_carrier`], and optionally apply the allowed classes.
//!
//! Without `--apply` the command reads GitHub and prints a plan; it holds no
//! mutation guard and issues no mutation. With `--apply` it executes only the
//! actions an intent file names, only for the classes passed with `--class`,
//! and only when a fresh plan made in the same invocation proposes the
//! identical action on the identical head. The intent file is written by the
//! controller before it calls `--apply`, so an interrupted pass leaves a
//! record naming the action it did not finish.
//!
//! `--replay` reads recorded facts instead of GitHub and prints their plans,
//! so a decision can be reproduced on any host from its recorded inputs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::native_arm::read_queue_state;
use super::observation::{gh_json, observe_repo, resolve_repos};
use super::{CliFailure, GitHubActions, ObservedPr, RepoObservation, write_json_envelope};
use crate::auto_arm::{arm_response_accepted, first_graphql_error};
use crate::identity::RuntimeMode;
use crate::merge_carrier::{
    CarrierAction, CarrierClass, CarrierFacts, CarrierPlan, CarrierQueueFact, JobFact,
    RequiredFact, RunFact, head_arm_time, plan,
};
use crate::merge_queue_control::MergeQueueMutationGuard;
use crate::merge_steward::selected_required_check;
use crate::paths::RuntimePaths;
use crate::pr_queue_state::{PrQueueState, explain_pr_queue_state};
use crate::ship_state::{ShipState, ShipStateStore};

/// Arm native auto-merge on one exact head.
const EXACT_HEAD_ARM_MUTATION: &str = "mutation($id:ID!,$head:GitObjectID!){\
     enablePullRequestAutoMerge(input:{pullRequestId:$id,mergeMethod:MERGE,\
     expectedHeadOid:$head}){pullRequest{number}}}";

/// Arguments for `shipyard runner carrier`.
#[derive(Clone, Debug)]
pub(crate) struct CarrierCommandArgs {
    pub(crate) repos: Vec<String>,
    pub(crate) base: String,
    pub(crate) classes: Vec<String>,
    pub(crate) intent: Option<PathBuf>,
    pub(crate) replay: Option<PathBuf>,
    pub(crate) apply: bool,
}

/// One plan plus the facts it was made from.
#[derive(Clone, Debug, Serialize)]
struct PlannedPr {
    #[serde(flatten)]
    plan: CarrierPlan,
    facts: CarrierFacts,
    #[serde(skip_serializing_if = "Option::is_none")]
    mutation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct CarrierRepoReport {
    repo: String,
    base: String,
    prs: Vec<PlannedPr>,
    errors: Vec<String>,
}

/// The actions a controller intends to apply, written before `--apply`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CarrierIntent {
    schema_version: u32,
    actions: Vec<IntentAction>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct IntentAction {
    repo: String,
    number: u64,
    head_sha: String,
    #[serde(flatten)]
    action: CarrierAction,
}

/// Parse and police the class list.
fn parse_classes(raw: &[String], apply: bool) -> Result<BTreeSet<CarrierClass>, CliFailure> {
    let mut classes = BTreeSet::new();
    for value in raw {
        let class = CarrierClass::parse(value).ok_or_else(|| {
            CliFailure::new(
                2,
                format!("unknown carrier class `{value}`: use redispatch, rearm, or update_branch"),
            )
        })?;
        classes.insert(class);
    }
    if apply && classes.is_empty() {
        return Err(CliFailure::new(
            2,
            "--apply requires at least one --class: classes graduate to live one at a time",
        ));
    }
    if apply && classes.contains(&CarrierClass::UpdateBranch) {
        return Err(CliFailure::new(
            2,
            "update_branch is planned only: applying it needs the own-lines invariant, which \
             extends the approval to the merge commit it creates",
        ));
    }
    Ok(classes)
}

pub(crate) fn carrier_command<W: Write>(
    args: &CarrierCommandArgs,
    cwd: &Path,
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let classes = parse_classes(&args.classes, args.apply)?;
    if let Some(path) = &args.replay {
        if args.apply {
            return Err(CliFailure::new(2, "--replay never applies"));
        }
        return replay_command(path, json_output, stdout);
    }
    let intent = if args.apply {
        let path = args.intent.as_ref().ok_or_else(|| {
            CliFailure::new(
                2,
                "--apply requires --intent: the controller records what it will do before it does it",
            )
        })?;
        Some(read_intent(path)?)
    } else {
        None
    };
    let store = if args.apply {
        Some(
            ShipStateStore::new(runtime_paths.state_dir.join("ship")).map_err(|error| {
                CliFailure::new(
                    1,
                    format!("could not open merge-queue mutation state: {error}"),
                )
            })?,
        )
    } else {
        None
    };
    let mut reports = Vec::new();
    let mut unhealthy = false;
    for repo in resolve_repos(args.repos.clone(), cwd)? {
        let mut report = CarrierRepoReport {
            repo: repo.clone(),
            base: args.base.clone(),
            prs: Vec::new(),
            errors: Vec::new(),
        };
        match observe_repo(actions, &repo, &args.base, false) {
            Ok(observation) => {
                report.repo.clone_from(&observation.repo);
                for pr in &observation.prs {
                    match carrier_facts(actions, &observation, pr) {
                        Ok(facts) => report.prs.push(PlannedPr {
                            plan: plan(&facts),
                            facts,
                            mutation: None,
                            error: None,
                        }),
                        Err(error) => report
                            .errors
                            .push(format!("PR #{}: {error}", pr.fact.number)),
                    }
                }
                if let (Some(intent), Some(store)) = (&intent, &store) {
                    apply_intent(
                        actions,
                        &observation,
                        intent,
                        &classes,
                        store,
                        mode,
                        &runtime_paths.global_dir,
                        &mut report,
                    );
                }
            }
            Err(error) => report.errors.push(error),
        }
        unhealthy |= !report.errors.is_empty() || report.prs.iter().any(|pr| pr.error.is_some());
        reports.push(report);
    }
    render(stdout, json_output, args.apply, &classes, &reports)?;
    Ok(if unhealthy {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn read_intent(path: &Path) -> Result<CarrierIntent, CliFailure> {
    let raw = fs::read_to_string(path).map_err(|error| {
        CliFailure::new(
            2,
            format!("could not read intent {}: {error}", path.display()),
        )
    })?;
    let intent: CarrierIntent = serde_json::from_str(&raw).map_err(|error| {
        CliFailure::new(
            2,
            format!("intent {} is malformed: {error}", path.display()),
        )
    })?;
    if intent.schema_version != 1 {
        return Err(CliFailure::new(2, "unsupported carrier intent schema"));
    }
    Ok(intent)
}

fn replay_command<W: Write>(
    path: &Path,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let raw = fs::read_to_string(path).map_err(|error| {
        CliFailure::new(
            2,
            format!("could not read replay facts {}: {error}", path.display()),
        )
    })?;
    let mut plans = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let facts: CarrierFacts = serde_json::from_str(line).map_err(|error| {
            CliFailure::new(
                2,
                format!("replay line {} is not carrier facts: {error}", index + 1),
            )
        })?;
        plans.push(
            json!({"repo": facts.repo, "observed_at": facts.observed_at, "plan": plan(&facts)}),
        );
    }
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("apply".to_owned(), Value::from(false));
        data.insert("replay".to_owned(), Value::from(path.display().to_string()));
        data.insert("plans".to_owned(), Value::Array(plans));
        write_json_envelope(stdout, "runner.carrier", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        for value in plans {
            writeln!(stdout, "{value}").map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn render<W: Write>(
    stdout: &mut W,
    json_output: bool,
    apply: bool,
    classes: &BTreeSet<CarrierClass>,
    reports: &[CarrierRepoReport],
) -> Result<(), CliFailure> {
    let failure = |error: std::io::Error| CliFailure::new(1, error.to_string());
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("apply".to_owned(), Value::from(apply));
        data.insert(
            "classes".to_owned(),
            serde_json::to_value(classes).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "repos".to_owned(),
            serde_json::to_value(reports).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        return write_json_envelope(stdout, "runner.carrier", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "carrier: mode={} classes={classes:?}",
        if apply { "apply" } else { "plan" }
    )
    .map_err(failure)?;
    for report in reports {
        writeln!(stdout, "{} base={}", report.repo, report.base).map_err(failure)?;
        for pr in &report.prs {
            let decision = serde_json::to_value(&pr.plan)
                .map_or_else(|_| "unprintable".to_owned(), |value| value.to_string());
            writeln!(stdout, "  {decision}").map_err(failure)?;
            if let Some(mutation) = &pr.mutation {
                writeln!(stdout, "    mutation: {mutation}").map_err(failure)?;
            }
            if let Some(error) = &pr.error {
                writeln!(stdout, "    error: {error}").map_err(failure)?;
            }
        }
        for error in &report.errors {
            writeln!(stdout, "  error: {error}").map_err(failure)?;
        }
    }
    Ok(())
}

/// Read every fact the planner needs for one pull request.
///
/// Calls are made only where the plan can depend on them: the queue timeline
/// for an unqueued pull request, approval only for an armed or removed one,
/// the head's runs only when a required check was interrupted, and the
/// removal's merge-group jobs only when a removal for failed checks names a
/// merge-group commit.
fn carrier_facts(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
) -> Result<CarrierFacts, String> {
    let fact = &pr.fact;
    let required: Vec<RequiredFact> = observation
        .required_checks
        .iter()
        .map(|rule| match selected_required_check(&fact.checks, rule) {
            Some(check) => RequiredFact {
                context: rule.context.clone(),
                status: check.status.to_ascii_uppercase(),
                conclusion: check.conclusion.as_deref().map(str::to_ascii_uppercase),
                run_id: check.run_id,
            },
            None => RequiredFact {
                context: rule.context.clone(),
                status: "MISSING".to_owned(),
                conclusion: None,
                run_id: None,
            },
        })
        .collect();
    let mut facts = CarrierFacts {
        repo: observation.repo.clone(),
        number: fact.number,
        head_sha: fact.head_sha.clone(),
        draft: fact.draft,
        merge_state: fact.merge_state.to_ascii_uppercase(),
        queue: if fact.queue_position.is_some() {
            CarrierQueueFact::Queued
        } else {
            CarrierQueueFact::Unknown {
                detail: "not read".to_owned(),
            }
        },
        approved_head: false,
        approval_evidence: None,
        review_marker: None,
        required,
        runs: Vec::new(),
        observed_at: Utc::now(),
    };
    if matches!(facts.queue, CarrierQueueFact::Queued) {
        return Ok(facts);
    }
    let response;
    (facts.queue, response) = queue_fact(actions, &observation.repo, fact.number, &fact.head_sha);
    if !matches!(
        facts.queue,
        CarrierQueueFact::ArmedNotQueued | CarrierQueueFact::Ejected { .. }
    ) {
        return Ok(facts);
    }
    if let Some(response) = &response {
        match head_arm_time(response, &fact.head_sha) {
            Ok(Some(at)) => {
                facts.approved_head = true;
                facts.approval_evidence = Some(format!("arm_event:{}", at.to_rfc3339()));
            }
            Ok(None) => {}
            Err(detail) => {
                facts.queue = CarrierQueueFact::Unknown { detail };
                return Ok(facts);
            }
        }
    }
    facts.review_marker = review_marker(actions, observation, pr)?;
    if !facts.approved_head {
        return Ok(facts);
    }
    let interrupted = facts.required.iter().any(|required| {
        required.conclusion.as_deref().is_some_and(|conclusion| {
            matches!(conclusion, "CANCELLED" | "STARTUP_FAILURE" | "STALE")
        })
    });
    if interrupted {
        facts.runs.extend(runs_for_head(
            actions,
            &observation.repo,
            &fact.head_sha,
            None,
        )?);
    }
    if let CarrierQueueFact::Ejected {
        reason,
        new_head_since: false,
        merge_group_commit: Some(commit),
    } = &facts.queue
        && reason.eq_ignore_ascii_case("failed_checks")
    {
        let mut group_runs =
            runs_for_head(actions, &observation.repo, commit, Some("merge_group"))?;
        for run in &mut group_runs {
            run.jobs = latest_jobs(actions, &observation.repo, run.id)?;
        }
        facts.runs.extend(group_runs);
    }
    Ok(facts)
}

fn queue_fact(
    actions: &GitHubActions,
    repo: &str,
    number: u64,
    head: &str,
) -> (CarrierQueueFact, Option<Value>) {
    let response = match read_queue_state(actions, repo, number) {
        Ok(response) => response,
        Err(detail) => return (CarrierQueueFact::Unknown { detail }, None),
    };
    (queue_fact_from(&response, head), Some(response))
}

fn queue_fact_from(response: &Value, head: &str) -> CarrierQueueFact {
    let report = explain_pr_queue_state(response);
    if !report
        .head_oid
        .as_deref()
        .is_some_and(|live| live.eq_ignore_ascii_case(head))
    {
        return CarrierQueueFact::Unknown {
            detail: "the head changed between observations".to_owned(),
        };
    }
    match report.state {
        PrQueueState::Merged | PrQueueState::Closed => CarrierQueueFact::NotOpen,
        PrQueueState::Queued { .. } => CarrierQueueFact::Queued,
        PrQueueState::ArmedNotQueued { .. } => CarrierQueueFact::ArmedNotQueued,
        PrQueueState::NeverArmed => CarrierQueueFact::NeverArmed,
        PrQueueState::Ejected {
            reason,
            new_head_since_removal,
            ..
        } => CarrierQueueFact::Ejected {
            reason,
            new_head_since: new_head_since_removal,
            merge_group_commit: report
                .last_ejection
                .and_then(|ejection| ejection.merge_group_commit),
        },
        PrQueueState::Unknown { detail } => CarrierQueueFact::Unknown { detail },
    }
}

/// The reviewer's `reviewed:<full sha>` cross-check for the current head.
///
/// Recorded beside the arming event for the audit trail; never trusted on its
/// own, because every agent posts through the same App identity.
fn review_marker(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
) -> Result<Option<String>, String> {
    let marker = Regex::new(r"(?im)^\s*reviewed:([0-9a-f]{40})\s*$").map_err(|e| e.to_string())?;
    for page in 1..=10 {
        let value = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{}/issues/{}/comments?per_page=100&page={page}",
                    observation.repo, pr.fact.number
                ),
            ],
            "pull-request comments",
        )?;
        let rows = value
            .as_array()
            .ok_or_else(|| "pull-request comments response is not a list".to_owned())?;
        for row in rows {
            let body = row.get("body").and_then(Value::as_str).unwrap_or_default();
            if marker
                .captures_iter(body)
                .any(|capture| capture[1].eq_ignore_ascii_case(&pr.fact.head_sha))
            {
                let id = row.get("id").and_then(Value::as_u64).unwrap_or_default();
                return Ok(Some(format!("comment:{id}")));
            }
        }
        if rows.len() < 100 {
            return Ok(None);
        }
    }
    Err("pull request has more than 1000 comments; refusing a partial approval scan".to_owned())
}

fn runs_for_head(
    actions: &GitHubActions,
    repo: &str,
    head: &str,
    event: Option<&str>,
) -> Result<Vec<RunFact>, String> {
    let mut path = format!("repos/{repo}/actions/runs?head_sha={head}&per_page=100");
    if let Some(event) = event {
        path.push_str("&event=");
        path.push_str(event);
    }
    let value = gh_json(actions, &["api".to_owned(), path], "workflow runs for head")?;
    let rows = value
        .get("workflow_runs")
        .and_then(Value::as_array)
        .ok_or_else(|| "workflow runs response missing workflow_runs".to_owned())?;
    if value
        .get("total_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 100
    {
        return Err(format!(
            "head {head} has more than 100 runs; refusing a partial read"
        ));
    }
    rows.iter().map(run_fact).collect()
}

fn run_fact(row: &Value) -> Result<RunFact, String> {
    let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
    Ok(RunFact {
        id: row
            .get("id")
            .and_then(Value::as_u64)
            .ok_or("run without id")?,
        workflow: text("path")
            .or_else(|| text("name"))
            .ok_or("run without workflow")?,
        event: text("event").ok_or("run without event")?,
        head_sha: text("head_sha").ok_or("run without head_sha")?,
        status: text("status").ok_or("run without status")?,
        conclusion: text("conclusion"),
        run_attempt: row
            .get("run_attempt")
            .and_then(Value::as_u64)
            .filter(|attempt| *attempt > 0)
            .ok_or("run without run_attempt")?,
        run_started_at: text("run_started_at")
            .and_then(|at| DateTime::parse_from_rfc3339(&at).ok())
            .map(|at| at.with_timezone(&Utc)),
        jobs: Vec::new(),
    })
}

fn time_field(row: &Value, key: &str) -> Option<DateTime<Utc>> {
    row.get(key)
        .and_then(Value::as_str)
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
}

fn latest_jobs(actions: &GitHubActions, repo: &str, run_id: u64) -> Result<Vec<JobFact>, String> {
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{repo}/actions/runs/{run_id}/jobs?filter=latest&per_page=100"),
        ],
        "merge-group jobs",
    )?;
    let rows = value
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("run {run_id} jobs response missing jobs"))?;
    if value
        .get("total_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 100
    {
        return Err(format!(
            "run {run_id} has more than 100 jobs; refusing a partial read"
        ));
    }
    Ok(rows
        .iter()
        .map(|row| JobFact {
            name: row
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            status: row
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            conclusion: row
                .get("conclusion")
                .and_then(Value::as_str)
                .map(str::to_owned),
            runner_name: row
                .get("runner_name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            created_at: time_field(row, "created_at"),
            completed_at: time_field(row, "completed_at"),
        })
        .collect())
}

/// Execute the intent's actions that the fresh plan still proposes.
#[allow(clippy::too_many_arguments)] // One apply pass needs every fence it checks.
fn apply_intent(
    actions: &GitHubActions,
    observation: &RepoObservation,
    intent: &CarrierIntent,
    classes: &BTreeSet<CarrierClass>,
    store: &ShipStateStore,
    mode: RuntimeMode,
    global_dir: &Path,
    report: &mut CarrierRepoReport,
) {
    for planned in &mut report.prs {
        let Some(action) = planned.plan.action().cloned() else {
            continue;
        };
        let wanted = IntentAction {
            repo: observation.repo.clone(),
            number: planned.plan.number,
            head_sha: planned.plan.head_sha.clone(),
            action: action.clone(),
        };
        if !intent.actions.contains(&wanted) {
            continue;
        }
        if !classes.contains(&action.class()) {
            planned.mutation = Some("not applied: class not enabled".to_owned());
            continue;
        }
        let Some(pr) = observation
            .prs
            .iter()
            .find(|pr| pr.fact.number == planned.plan.number)
        else {
            continue;
        };
        let state = ShipState::new(
            pr.fact.number,
            &observation.repo,
            &pr.fact.head_branch,
            &observation.base,
            &pr.fact.head_sha,
            "runner-carrier",
        );
        let outcome = match &action {
            CarrierAction::Redispatch { run_ids, .. } => {
                redispatch(actions, store, mode, global_dir, &state, run_ids)
            }
            CarrierAction::Rearm { head } => {
                rearm(actions, store, mode, global_dir, &state, &pr.node_id, head)
            }
            CarrierAction::UpdateBranch { .. } => Err("update_branch is planned only".to_owned()),
        };
        match outcome {
            Ok(mutation) => planned.mutation = Some(mutation),
            Err(error) => planned.error = Some(error),
        }
    }
}

fn redispatch(
    actions: &GitHubActions,
    store: &ShipStateStore,
    mode: RuntimeMode,
    global_dir: &Path,
    state: &ShipState,
    run_ids: &[u64],
) -> Result<String, String> {
    let mut done = Vec::new();
    for run_id in run_ids {
        let guard = MergeQueueMutationGuard::acquire_in_mode(
            store,
            Path::new("."),
            mode,
            global_dir,
            state,
            &format!("runner carrier redispatch run {run_id}"),
        )?;
        match actions.rerun_failed_run(&state.repo, *run_id) {
            Ok(()) => {
                guard.finish("rerun_accepted")?;
                done.push(*run_id);
            }
            Err(error) => {
                guard.finish("rerun_rejected")?;
                return Err(format!(
                    "rerun of run {run_id} was rejected after {done:?}: {error}"
                ));
            }
        }
    }
    Ok(format!("reran {done:?}"))
}

fn rearm(
    actions: &GitHubActions,
    store: &ShipStateStore,
    mode: RuntimeMode,
    global_dir: &Path,
    state: &ShipState,
    node_id: &str,
    head: &str,
) -> Result<String, String> {
    if node_id.is_empty() {
        return Err("pull request node ID unavailable".to_owned());
    }
    let guard = MergeQueueMutationGuard::acquire_in_mode(
        store,
        Path::new("."),
        mode,
        global_dir,
        state,
        "runner carrier rearm exact head",
    )?;
    // The carrier's plan is the head-scoped verdict here: it re-arms only a
    // head the queue removed for starvation, which the ghapp guard would
    // refuse as a same-head failed_checks re-arm without seeing the jobs.
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={EXACT_HEAD_ARM_MUTATION}"),
        "-F".to_owned(),
        format!("id={node_id}"),
        "-F".to_owned(),
        format!("head={head}"),
    ];
    match actions.run_gh_internal_queue_mutation(&args) {
        Ok(raw) if arm_response_accepted(&raw) => {
            guard.finish("armed")?;
            Ok(format!("armed {head} with expectedHeadOid"))
        }
        Ok(raw) => {
            guard.finish("arm_rejected")?;
            Err(format!(
                "arm returned no armed pull request: {}",
                first_graphql_error(&raw).unwrap_or_else(|| "no errors reported".to_owned())
            ))
        }
        Err(error) => {
            guard.finish("arm_failed")?;
            Err(format!("arm failed: {error}"))
        }
    }
}

#[cfg(test)]
mod tests;
