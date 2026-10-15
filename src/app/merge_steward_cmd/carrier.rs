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

/// Disarm native auto-merge; the answer names any request left armed.
const DISARM_MUTATION: &str = "mutation($id:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$id}){pullRequest{number autoMergeRequest{enabledAt}}}}";

/// Arguments for `shipyard runner carrier`.
#[derive(Clone, Debug)]
pub(crate) struct CarrierCommandArgs {
    pub(crate) repos: Vec<String>,
    pub(crate) base: String,
    pub(crate) classes: Vec<String>,
    pub(crate) intent: Option<PathBuf>,
    pub(crate) replay: Option<PathBuf>,
    /// `OWNER/REPO=PATH` clones where `update_branch` reads its git evidence.
    pub(crate) clones: Vec<String>,
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
    let clones = parse_clones(&args.clones)?;
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
                        cwd,
                        &clones,
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
        carry_approval: None,
        carry_approval_detail: None,
        carries_last_day: None,
        head_is_carry: false,
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
    read_carried_head(actions, observation, pr, response.as_ref(), &mut facts)?;
    if facts
        .required
        .iter()
        .any(|required| required.conclusion.as_deref() == Some("CANCELLED"))
    {
        facts
            .runs
            .extend(interrupted_head_runs(actions, &observation.repo, &facts)?);
    }
    let group_runs = removal_group_runs(actions, &observation.repo, &facts.queue)?;
    facts.runs.extend(group_runs);
    if matches!(facts.queue, CarrierQueueFact::ArmedNotQueued)
        && facts.merge_state == "BEHIND"
        && let Some(queue) = &response
    {
        read_carry_facts(actions, observation, pr, queue, &mut facts)?;
    }
    Ok(facts)
}

/// The facts only a carry needs: the reviewer approval of this head, every
/// run on it (a live one holds the carry), and how many merge-only updates
/// GitHub made on the pull request in the last day.
fn read_carry_facts(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    queue: &Value,
    facts: &mut CarrierFacts,
) -> Result<(), String> {
    if facts.carry_approval.is_none() && facts.carry_approval_detail.is_none() {
        read_head_approval(actions, observation, pr, queue, facts)?;
    }
    let repo = &observation.repo;
    if facts.runs.is_empty() {
        facts
            .runs
            .extend(runs_for_head(actions, repo, &pr.fact.head_sha, None)?);
    }
    facts.carries_last_day = Some(github_carries_last_day(
        actions,
        repo,
        pr.fact.number,
        &observation.base,
        facts.observed_at,
    )?);
    Ok(())
}

/// The merge-group runs, with jobs, of a same-head removal for failed checks.
fn removal_group_runs(
    actions: &GitHubActions,
    repo: &str,
    queue: &CarrierQueueFact,
) -> Result<Vec<RunFact>, String> {
    let CarrierQueueFact::Ejected {
        reason,
        new_head_since: false,
        merge_group_commit: Some(commit),
    } = queue
    else {
        return Ok(Vec::new());
    };
    if !reason.eq_ignore_ascii_case("failed_checks") {
        return Ok(Vec::new());
    }
    let mut group_runs = runs_for_head(actions, repo, commit, Some("merge_group"))?;
    for run in &mut group_runs {
        run.jobs = latest_jobs(actions, repo, run.id, false)?;
    }
    Ok(group_runs)
}

/// Whether the head is a branch-update merge and, when it is, the reviewer
/// approval it needs of its own.
fn read_carried_head(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    queue: Option<&Value>,
    facts: &mut CarrierFacts,
) -> Result<(), String> {
    facts.head_is_carry = head_is_branch_update(
        actions,
        &observation.repo,
        &observation.base,
        &pr.fact.head_sha,
    )?;
    if facts.head_is_carry
        && let Some(queue) = queue
    {
        read_head_approval(actions, observation, pr, queue, facts)?;
    }
    Ok(())
}

/// The reviewer approval of the current head through [`crate::head_approval`],
/// into [`CarrierFacts::carry_approval`], or why there is none.
fn read_head_approval(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    queue: &Value,
    facts: &mut CarrierFacts,
) -> Result<(), String> {
    let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
    let repo = &observation.repo;
    match crate::head_approval::read_policy_required(&run_gh, repo, &observation.base) {
        Ok(policy) => match crate::head_approval::evaluate_with_policy_for_author_with_events(
            &run_gh,
            repo,
            pr.fact.number,
            &pr.fact.head_sha,
            &policy,
            queue,
            pr.fact.author.as_deref(),
        )? {
            (crate::head_approval::HeadGate::Approved { approval }, _) => {
                facts.carry_approval = Some(approval_evidence(&approval));
            }
            (crate::head_approval::HeadGate::NotRequired, _) => {
                facts.carry_approval_detail = Some(
                    "the repository does not require head approval, and a carry extends one"
                        .to_owned(),
                );
            }
            (gate, _) => facts.carry_approval_detail = Some(gate.explain()),
        },
        Err(error) => facts.carry_approval_detail = Some(error),
    }
    Ok(())
}

/// Whether `head` is a merge commit GitHub made for an `update-branch`:
/// two parents, committed by GitHub, `Merge branch '<base>' into ...`. Such a
/// head was armed by whoever carried it, so its arm is not a reviewer's
/// approval of it.
fn head_is_branch_update(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    head: &str,
) -> Result<bool, String> {
    let commit = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{repo}/commits/{head}"),
            "--jq".to_owned(),
            "{parents: [.parents[].sha], committer_email: .commit.committer.email, message: \
             .commit.message}"
                .to_owned(),
        ],
        "head commit",
    )?;
    Ok(is_branch_update_commit(&commit, base))
}

fn is_branch_update_commit(commit: &Value, base: &str) -> bool {
    commit
        .get("parents")
        .and_then(Value::as_array)
        .is_some_and(|parents| parents.len() == 2)
        && commit.get("committer_email").and_then(Value::as_str) == Some("noreply@github.com")
        && commit
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.starts_with(&format!("Merge branch '{base}' into ")))
}

/// One line naming the approval a carry extends, for the plan and the audit.
fn approval_evidence(approval: &crate::head_approval::Approval) -> String {
    let app = |id: &Option<u64>| id.map_or_else(String::new, |id| format!(" app={id}"));
    match approval {
        crate::head_approval::Approval::Review {
            login,
            performed_via_github_app_id,
        } => format!("review:{login}{}", app(performed_via_github_app_id)),
        crate::head_approval::Approval::Marker {
            login,
            comment_id,
            performed_via_github_app_id,
        } => format!(
            "marker:{login} comment={}{}",
            comment_id.map_or_else(|| "unknown".to_owned(), |id| id.to_string()),
            app(performed_via_github_app_id)
        ),
        crate::head_approval::Approval::Carried {
            from_head,
            approval,
            rebase,
        } => format!(
            "carried:{} from={from_head}{}",
            approval_evidence(approval),
            if *rebase { " via=rebase" } else { "" }
        ),
    }
}

/// Merge-only branch updates on the pull request in the 24 hours before
/// `now`, read from its commits: two parents, committed by GitHub, with
/// GitHub's `Merge branch '<base>' into` message.
fn github_carries_last_day(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    base: &str,
    now: DateTime<Utc>,
) -> Result<u32, String> {
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            "--paginate".to_owned(),
            format!("repos/{repo}/pulls/{pr}/commits?per_page=100"),
            "--jq".to_owned(),
            ".[] | {parents: (.parents | length), message: .commit.message, email: \
             .commit.committer.email, date: .commit.committer.date}"
                .to_owned(),
        ])
        .map_err(|error| format!("pull-request commits unreadable: {error}"))?;
    let prefix = format!("Merge branch '{base}' into ");
    let cutoff = now - chrono::Duration::hours(24);
    let mut carries = 0;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let row: Value = serde_json::from_str(line)
            .map_err(|error| format!("malformed pull-request commit record: {error}"))?;
        let merged_by_github = row.get("parents").and_then(Value::as_u64) == Some(2)
            && row.get("email").and_then(Value::as_str) == Some("noreply@github.com")
            && row
                .get("message")
                .and_then(Value::as_str)
                .is_some_and(|message| message.starts_with(&prefix));
        let recent = time_field(&row, "date").is_some_and(|at| at >= cutoff);
        if merged_by_github && recent {
            carries += 1;
        }
    }
    Ok(carries)
}

/// The head's runs, with the latest-attempt jobs of every run a cancelled
/// required check names: the interruption signature is read from them.
fn interrupted_head_runs(
    actions: &GitHubActions,
    repo: &str,
    facts: &CarrierFacts,
) -> Result<Vec<RunFact>, String> {
    let interrupted: BTreeSet<u64> = facts
        .required
        .iter()
        .filter(|required| required.conclusion.as_deref() == Some("CANCELLED"))
        .filter_map(|required| required.run_id)
        .collect();
    let mut head_runs = runs_for_head(actions, repo, &facts.head_sha, None)?;
    for run in &mut head_runs {
        if interrupted.contains(&run.id) {
            run.jobs = latest_jobs(actions, repo, run.id, true)?;
        }
    }
    Ok(head_runs)
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

/// Jobs of a run's latest attempt. With `read_supersede`, each runner-less
/// cancellation's check-run annotations are read for a concurrency supersede.
fn latest_jobs(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
    read_supersede: bool,
) -> Result<Vec<JobFact>, String> {
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
    rows.iter()
        .map(|row| {
            let mut job = job_fact(row);
            let runnerless = job.conclusion.as_deref() == Some("cancelled")
                && job
                    .runner_name
                    .as_deref()
                    .is_none_or(|name| name.trim().is_empty());
            if read_supersede && runnerless {
                let job_id = row
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| format!("run {run_id} has a job without an id"))?;
                let annotations = gh_json(
                    actions,
                    &[
                        "api".to_owned(),
                        format!("repos/{repo}/check-runs/{job_id}/annotations"),
                    ],
                    "job annotations",
                )?;
                let mut marks = job.interruption_job();
                crate::transient_interruption::mark_from_annotations(&mut marks, &annotations);
                job.superseded = marks.superseded;
                job.cancelled_by_actor = marks.cancelled_by_actor;
            }
            Ok(job)
        })
        .collect()
}

fn job_fact(row: &Value) -> JobFact {
    JobFact {
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
        superseded: false,
        cancelled_by_actor: false,
    }
}

/// Execute the intent's actions that the fresh plan still proposes.
#[allow(clippy::too_many_arguments)] // One apply pass needs every fence it checks.
fn apply_intent(
    actions: &GitHubActions,
    observation: &RepoObservation,
    cwd: &Path,
    clones: &BTreeMap<String, PathBuf>,
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
            CarrierAction::UpdateBranch { head } => update_branch(
                actions,
                store,
                mode,
                global_dir,
                carry::resolve_clone(clones, cwd, &observation.repo),
                &state,
                &pr.node_id,
                head,
                planned,
            ),
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

/// Parse `--clone OWNER/REPO=PATH` values.
fn parse_clones(raw: &[String]) -> Result<BTreeMap<String, PathBuf>, CliFailure> {
    raw.iter()
        .map(|value| {
            let (repo, path) = value
                .split_once('=')
                .filter(|(repo, path)| repo.contains('/') && !path.is_empty())
                .ok_or_else(|| {
                    CliFailure::new(2, format!("--clone `{value}` is not OWNER/REPO=PATH"))
                })?;
            Ok((repo.to_owned(), PathBuf::from(path)))
        })
        .collect()
}

/// Carries started for this pull request in the last 24 hours, from the
/// local mutation audit. Counts started attempts, so a carry interrupted
/// between the update and its audit record still counts. GitHub's own record
/// is [`CarrierFacts::carries_last_day`]; this only ever makes the limit
/// stricter.
fn carries_started_today(store: &ShipStateStore, repo: &str, pr: u64) -> usize {
    let path = store
        .path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("merge_queue/mutations.jsonl");
    let Ok(raw) = fs::read_to_string(path) else {
        return 0;
    };
    let cutoff = Utc::now() - chrono::Duration::hours(24);
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| {
            row.get("action").and_then(Value::as_str) == Some(CARRY_ACTION)
                && row.get("repo").and_then(Value::as_str) == Some(repo)
                && row.get("pr").and_then(Value::as_u64) == Some(pr)
                && row
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(|stamp| DateTime::parse_from_rfc3339(stamp).ok())
                    .is_some_and(|stamp| stamp.with_timezone(&Utc) >= cutoff)
        })
        .filter_map(|row| {
            row.get("correlation_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>()
        .len()
}

const CARRY_ACTION: &str = "runner carrier update branch";

/// Re-check the plan's carry facts at apply time and return the approval the
/// carry extends.
fn carry_preconditions<'a>(
    store: &ShipStateStore,
    state: &ShipState,
    head: &str,
    facts: &'a CarrierFacts,
) -> Result<&'a str, String> {
    if !head.eq_ignore_ascii_case(&state.head_sha) || !head.eq_ignore_ascii_case(&facts.head_sha) {
        return Err("carry head differs from the observed exact head".to_owned());
    }
    let Some(approval) = facts.carry_approval.as_deref() else {
        return Err("carry requires a reviewer approval of the current head".to_owned());
    };
    if !facts.merge_state.eq_ignore_ascii_case("BEHIND") {
        return Err(format!(
            "carry requires a BEHIND pull request (merge state {})",
            facts.merge_state
        ));
    }
    let started = carries_started_today(store, &state.repo, state.pr);
    if started >= crate::merge_carrier::MAX_CARRIES_PER_DAY as usize {
        return Err(format!(
            "carry budget spent: {started} carries of this pull request started in 24 hours"
        ));
    }
    Ok(approval)
}

/// Merge the base into an approved, armed, `BEHIND` head and re-arm the
/// carried head.
///
/// The plan made in this invocation already holds every row-2 limit; this
/// re-checks the ones the plan's facts carry, then predicts the merge in the
/// repository's own clone before the update and verifies the commit GitHub
/// created after it. Any mismatch leaves the pull request disarmed.
#[allow(clippy::too_many_arguments)] // One carry needs every fence it checks.
fn update_branch(
    actions: &GitHubActions,
    store: &ShipStateStore,
    mode: RuntimeMode,
    global_dir: &Path,
    clone: Result<PathBuf, String>,
    state: &ShipState,
    node_id: &str,
    head: &str,
    planned: &PlannedPr,
) -> Result<String, String> {
    let approval = carry_preconditions(store, state, head, &planned.facts)?;
    let clone = clone?;
    let live_base = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{}/branches/{}", state.repo, state.base_branch),
        ],
        "live base branch",
    )?;
    let live_base_sha = live_base
        .pointer("/commit/sha")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{} has no readable head commit", state.base_branch))?;
    let prediction = carry::predict(&clone, &state.base_branch, state.pr, head, live_base_sha)?;
    let mut guard = MergeQueueMutationGuard::acquire_in_mode(
        store,
        Path::new("."),
        mode,
        global_dir,
        state,
        CARRY_ACTION,
    )?;
    guard.record_verdict_audit_events(vec![
        format!("approval:{approval}"),
        format!("approved_head:{head}"),
        format!("base:{}", prediction.base_sha),
        format!("merge_tree:{}", prediction.carry_tree),
        format!("own_lines_sha256:{}", prediction.own_lines_sha256),
        "update_method:merge".to_owned(),
    ])?;
    // GitHub keeps auto-merge armed through an update-branch made by an
    // account with write access, so the new head would merge on green
    // before it is verified. Disarm first; only a verified head is armed.
    if let Err(error) = disarm(actions, node_id) {
        guard.finish("carry_disarm_failed")?;
        return Err(format!("could not disarm before the carry: {error}"));
    }
    let args = vec![
        "api".to_owned(),
        "--method".to_owned(),
        "PUT".to_owned(),
        format!("repos/{}/pulls/{}/update-branch", state.repo, state.pr),
        "-f".to_owned(),
        format!("expected_head_sha={head}"),
        "-f".to_owned(),
        "update_method=merge".to_owned(),
    ];
    if let Err(error) = actions.run_gh_internal_queue_mutation(&args) {
        guard.finish("carry_failed")?;
        // Nothing moved: the approved head is armed again as it was.
        let restored = rearm(actions, store, mode, global_dir, state, node_id, head)
            .unwrap_or_else(|rearm_error| format!("re-arm failed, left disarmed: {rearm_error}"));
        return Err(format!("update branch failed: {error}; {restored}"));
    }
    let Some(new_head) = carried_head(actions, state, head) else {
        guard.finish("carry_head_unobserved")?;
        return Err(format!(
            "update branch was accepted but the pull request head never moved from {head}; \
             left disarmed"
        ));
    };
    let tree = match carry::verify(
        &clone,
        &state.base_branch,
        state.pr,
        head,
        &new_head,
        &prediction,
    ) {
        Ok(tree) => tree,
        Err(error) => {
            guard.finish("carry_verification_failed")?;
            return Err(format!(
                "carried {head} to {new_head}, left disarmed: {error}"
            ));
        }
    };
    guard.finish("carried")?;
    let carried_state = ShipState::new(
        state.pr,
        &state.repo,
        &state.branch,
        &state.base_branch,
        new_head.clone(),
        "runner-carrier",
    );
    let arm = rearm(
        actions,
        store,
        mode,
        global_dir,
        &carried_state,
        node_id,
        &new_head,
    )?;
    Ok(format!(
        "carried {head} to {new_head} with merge tree {tree} ({approval}); {arm}"
    ))
}

/// Disarm native auto-merge. Accepted only when GitHub answers with the pull
/// request and no auto-merge request on it.
fn disarm(actions: &GitHubActions, node_id: &str) -> Result<(), String> {
    if node_id.is_empty() {
        return Err("pull request node ID unavailable".to_owned());
    }
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={DISARM_MUTATION}"),
        "-F".to_owned(),
        format!("id={node_id}"),
    ];
    let raw = actions
        .run_gh_internal_queue_mutation(&args)
        .map_err(|error| error.to_string())?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|error| format!("disarm answer unreadable: {error}"))?;
    let pull = value
        .pointer("/data/disablePullRequestAutoMerge/pullRequest")
        .filter(|pull| pull.is_object())
        .ok_or_else(|| {
            first_graphql_error(&raw).unwrap_or_else(|| "no pull request in the answer".to_owned())
        })?;
    if pull
        .get("autoMergeRequest")
        .is_some_and(|request| !request.is_null())
    {
        return Err("auto-merge is still armed after the disarm".to_owned());
    }
    Ok(())
}

/// The pull request's head once GitHub has applied the update, read until it
/// moves from `head` or the attempts run out.
fn carried_head(actions: &GitHubActions, state: &ShipState, head: &str) -> Option<String> {
    for attempt in 0..carry::HEAD_POLL_ATTEMPTS {
        if attempt > 0 {
            carry::poll_pause();
        }
        let Ok(pull) = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!("repos/{}/pulls/{}", state.repo, state.pr),
            ],
            "carried pull request head",
        ) else {
            continue;
        };
        if let Some(sha) = pull
            .pointer("/head/sha")
            .and_then(Value::as_str)
            .filter(|sha| crate::merge_steward::is_full_sha(sha) && !sha.eq_ignore_ascii_case(head))
        {
            return Some(sha.to_owned());
        }
    }
    None
}

mod carry;
#[cfg(test)]
mod tests;
