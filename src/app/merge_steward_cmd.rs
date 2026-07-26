use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CliFailure;
use crate::cloud::GitHubActions;
use crate::merge_steward::{
    RunCancellation, StewardCheck, StewardDecision, StewardPolicy, StewardPullRequest, StewardRun,
    classify_pr, is_full_sha, plan_run_coalescing,
};
use crate::output::write_json_envelope;

pub(super) struct StewardCommandArgs {
    pub(super) repos: Vec<String>,
    pub(super) base: String,
    pub(super) opt_out_label: String,
    pub(super) max_transient_reruns: u32,
    pub(super) coalesce: bool,
    pub(super) apply: bool,
    pub(super) ledger: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct ObservedPr {
    node_id: String,
    fact: StewardPullRequest,
}

#[derive(Clone, Debug)]
struct RepoObservation {
    repo: String,
    allow_auto_merge: bool,
    merge_queue: bool,
    required_contexts: Vec<String>,
    prs: Vec<ObservedPr>,
    runs: Vec<StewardRun>,
    merge_group_heads: BTreeMap<u64, String>,
}

type MergeQueueSnapshot = (bool, BTreeMap<u64, u64>, BTreeMap<u64, String>);

#[derive(Clone, Debug, Serialize)]
struct PrReport {
    number: u64,
    head_sha: String,
    decision: StewardDecision,
    mutation: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct CancellationReport {
    run_id: u64,
    reason: String,
    mutation: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct RepoReport {
    repo: String,
    base: String,
    allow_auto_merge: bool,
    merge_queue: bool,
    merge_path: String,
    required_contexts: Vec<String>,
    prs: Vec<PrReport>,
    cancellations: Vec<CancellationReport>,
    errors: Vec<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct StewardLedger {
    #[serde(default)]
    transient_attempts: BTreeMap<String, u32>,
    #[serde(default)]
    audit: Vec<LedgerAudit>,
}

#[derive(Deserialize, Serialize)]
struct LedgerAudit {
    at: String,
    repo: String,
    subject: String,
    action: String,
}

pub(super) fn steward_command<W: Write>(
    args: &StewardCommandArgs,
    cwd: &Path,
    state_dir: &Path,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repos = resolve_repos(args.repos.clone(), cwd)?;
    let ledger_path = args
        .ledger
        .clone()
        .unwrap_or_else(|| state_dir.join("merge-steward.json"));
    let mut ledger = load_ledger(&ledger_path)?;
    let mut reports = Vec::new();
    let mut unhealthy = false;
    for repo in repos {
        match observe_repo(actions, &repo, &args.base) {
            Ok(observation) => {
                let (report, failed) = apply_repo_plan(actions, args, &observation, &mut ledger);
                unhealthy |= failed;
                reports.push(report);
            }
            Err(error) => {
                unhealthy = true;
                reports.push(RepoReport {
                    repo,
                    base: args.base.clone(),
                    allow_auto_merge: false,
                    merge_queue: false,
                    merge_path: "unreadable".to_owned(),
                    required_contexts: Vec::new(),
                    prs: Vec::new(),
                    cancellations: Vec::new(),
                    errors: vec![error],
                });
            }
        }
    }
    if args.apply {
        save_ledger(&ledger_path, &ledger)?;
    }
    render_report(stdout, json_output, args.apply, &reports)?;
    Ok(if unhealthy {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn resolve_repos(mut repos: Vec<String>, cwd: &Path) -> Result<Vec<String>, CliFailure> {
    if repos.is_empty() {
        repos.push(super::runner_cmd::resolve_repo_slug(None, cwd)?);
    }
    repos.sort();
    repos.dedup();
    for repo in &repos {
        let Some((owner, name)) = repo.split_once('/') else {
            return Err(CliFailure::new(
                1,
                format!("invalid repository slug `{repo}`"),
            ));
        };
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return Err(CliFailure::new(
                1,
                format!("invalid repository slug `{repo}`"),
            ));
        }
    }
    Ok(repos)
}

fn observe_repo(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<RepoObservation, String> {
    let settings = gh_json(
        actions,
        &["api".to_owned(), format!("repos/{repo}")],
        "repository settings",
    )?;
    let allow_auto_merge = settings
        .get("allow_auto_merge")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let required_contexts = required_contexts(actions, repo, base)?;
    let (merge_queue, queue_positions, merge_group_heads) =
        merge_queue_snapshot(actions, repo, base)?;
    let prs = pull_requests(actions, repo, base, &queue_positions)?;
    let runs = active_runs(actions, repo)?;
    Ok(RepoObservation {
        repo: repo.to_owned(),
        allow_auto_merge,
        merge_queue,
        required_contexts,
        prs,
        runs,
        merge_group_heads,
    })
}

fn gh_json(actions: &GitHubActions, args: &[String], purpose: &str) -> Result<Value, String> {
    let raw = actions
        .run_gh(args)
        .map_err(|error| format!("{purpose} failed: {error}"))?;
    serde_json::from_str(&raw)
        .map_err(|error| format!("{purpose} returned malformed JSON: {error}"))
}

fn required_contexts(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Vec<String>, String> {
    let args = vec![
        "api".to_owned(),
        format!("repos/{repo}/branches/{base}/protection/required_status_checks"),
    ];
    let raw = match actions.run_gh(&args) {
        Ok(raw) => raw,
        Err(error) if is_private_free_entitlement(&error.to_string()) => return Ok(Vec::new()),
        Err(error) if error.to_string().contains("HTTP 404") => return Ok(Vec::new()),
        Err(error) => return Err(format!("required-check policy read failed: {error}")),
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("required-check policy returned malformed JSON: {error}"))?;
    let mut contexts = value
        .get("contexts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Some(checks) = value.get("checks").and_then(Value::as_array) {
        contexts.extend(checks.iter().filter_map(|check| {
            check
                .get("context")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }));
    }
    contexts.sort();
    contexts.dedup();
    Ok(contexts)
}

fn merge_queue_snapshot(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<MergeQueueSnapshot, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository slug `{repo}`"))?;
    let query = "query($owner:String!,$name:String!,$branch:String!){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){entries(first:100){nodes{position headCommit{oid} pullRequest{number}} pageInfo{hasNextPage}}}}}";
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("branch={base}"),
    ];
    let value = match gh_json(actions, &args, "merge-queue policy") {
        Ok(value) => value,
        Err(error) if is_private_free_entitlement(&error) => {
            return Ok((false, BTreeMap::new(), BTreeMap::new()));
        }
        Err(error) => return Err(error),
    };
    if value
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        let text = value.to_string();
        if is_private_free_entitlement(&text) {
            return Ok((false, BTreeMap::new(), BTreeMap::new()));
        }
        return Err(format!("merge-queue GraphQL errors: {text}"));
    }
    let Some(queue) = value.pointer("/data/repository/mergeQueue") else {
        return Err("merge-queue response missing repository.mergeQueue".to_owned());
    };
    if queue.is_null() {
        return Ok((false, BTreeMap::new(), BTreeMap::new()));
    }
    if queue
        .pointer("/entries/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err("merge queue exceeds 100 entries; refusing a partial snapshot".to_owned());
    }
    let nodes = queue
        .pointer("/entries/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "merge-queue response missing entries.nodes".to_owned())?;
    let mut positions = BTreeMap::new();
    let mut heads = BTreeMap::new();
    for node in nodes {
        let Some(number) = node.pointer("/pullRequest/number").and_then(Value::as_u64) else {
            return Err("merge-queue entry missing PR number".to_owned());
        };
        let Some(position) = node.get("position").and_then(Value::as_u64) else {
            return Err(format!("merge-queue PR #{number} missing position"));
        };
        positions.insert(number, position);
        if let Some(head) = node
            .pointer("/headCommit/oid")
            .and_then(Value::as_str)
            .filter(|head| is_full_sha(head))
        {
            heads.insert(number, head.to_owned());
        }
    }
    Ok((true, positions, heads))
}

fn pull_requests(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    positions: &BTreeMap<u64, u64>,
) -> Result<Vec<ObservedPr>, String> {
    let args = vec![
        "pr".to_owned(),
        "list".to_owned(),
        "--repo".to_owned(),
        repo.to_owned(),
        "--state".to_owned(),
        "open".to_owned(),
        "--base".to_owned(),
        base.to_owned(),
        "--limit".to_owned(),
        "100".to_owned(),
        "--json".to_owned(),
        "id,number,isDraft,headRefOid,headRefName,mergeStateStatus,autoMergeRequest,labels,statusCheckRollup".to_owned(),
    ];
    let value = gh_json(actions, &args, "open PR list")?;
    let rows = value
        .as_array()
        .ok_or_else(|| "open PR list was not an array".to_owned())?;
    if rows.len() == 100 {
        return Err("open PR list reached 100; refusing a possibly partial snapshot".to_owned());
    }
    rows.iter().map(|row| parse_pr(row, positions)).collect()
}

fn parse_pr(row: &Value, positions: &BTreeMap<u64, u64>) -> Result<ObservedPr, String> {
    let number = row
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| "PR row missing number".to_owned())?;
    let node_id = row
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| format!("PR #{number} missing node ID"))?
        .to_owned();
    let checks = row
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_check)
        .collect();
    Ok(ObservedPr {
        node_id,
        fact: StewardPullRequest {
            number,
            head_sha: row
                .get("headRefOid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            head_branch: row
                .get("headRefName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            draft: row.get("isDraft").and_then(Value::as_bool).unwrap_or(true),
            merge_state: row
                .get("mergeStateStatus")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN")
                .to_owned(),
            auto_merge_active: row
                .get("autoMergeRequest")
                .is_some_and(|request| !request.is_null()),
            queue_position: positions.get(&number).copied(),
            labels: row
                .get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|label| label.get("name").and_then(Value::as_str))
                .map(str::to_owned)
                .collect(),
            checks,
        },
    })
}

fn parse_check(value: &Value) -> Option<StewardCheck> {
    match value.get("__typename").and_then(Value::as_str)? {
        "CheckRun" => Some(StewardCheck {
            name: value.get("name")?.as_str()?.to_owned(),
            status: value.get("status")?.as_str()?.to_owned(),
            conclusion: value
                .get("conclusion")
                .and_then(Value::as_str)
                .filter(|conclusion| !conclusion.is_empty())
                .map(str::to_owned),
            run_id: value
                .get("detailsUrl")
                .and_then(Value::as_str)
                .and_then(run_id_from_url),
            observed_at: value
                .get("completedAt")
                .or_else(|| value.get("startedAt"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        "StatusContext" => {
            let state = value.get("state")?.as_str()?;
            Some(StewardCheck {
                name: value.get("context")?.as_str()?.to_owned(),
                status: if matches!(state, "PENDING" | "EXPECTED") {
                    "IN_PROGRESS"
                } else {
                    "COMPLETED"
                }
                .to_owned(),
                conclusion: match state {
                    "SUCCESS" => Some("SUCCESS".to_owned()),
                    "ERROR" | "FAILURE" => Some("FAILURE".to_owned()),
                    _ => None,
                },
                run_id: value
                    .get("targetUrl")
                    .and_then(Value::as_str)
                    .and_then(run_id_from_url),
                observed_at: value
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        }
        _ => None,
    }
}

fn run_id_from_url(url: &str) -> Option<u64> {
    let tail = url.split("/actions/runs/").nth(1)?;
    tail.split('/').next()?.parse().ok()
}

fn active_runs(actions: &GitHubActions, repo: &str) -> Result<Vec<StewardRun>, String> {
    let mut all = Vec::new();
    for status in ["queued", "in_progress"] {
        for page in 1..=10 {
            let value = gh_json(
                actions,
                &[
                    "api".to_owned(),
                    format!("repos/{repo}/actions/runs?status={status}&per_page=100&page={page}"),
                ],
                "active workflow runs",
            )?;
            let rows = value
                .get("workflow_runs")
                .and_then(Value::as_array)
                .ok_or_else(|| "active workflow response missing workflow_runs".to_owned())?;
            let count = rows.len();
            all.extend(rows.iter().filter_map(parse_run));
            if count < 100 {
                break;
            }
            if page == 10 {
                return Err("active workflow runs exceed 1000; refusing partial scan".to_owned());
            }
        }
    }
    Ok(all)
}

fn parse_run(value: &Value) -> Option<StewardRun> {
    Some(StewardRun {
        id: value.get("id")?.as_u64()?,
        workflow_id: value.get("workflow_id")?.as_u64()?,
        head_sha: value.get("head_sha")?.as_str()?.to_owned(),
        head_branch: value.get("head_branch")?.as_str()?.to_owned(),
        status: value.get("status")?.as_str()?.to_owned(),
        event: value.get("event")?.as_str()?.to_owned(),
        created_at: value.get("created_at")?.as_str()?.to_owned(),
    })
}

fn apply_repo_plan(
    actions: &GitHubActions,
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    ledger: &mut StewardLedger,
) -> (RepoReport, bool) {
    let policy = StewardPolicy {
        merge_queue: observation.merge_queue,
        native_auto_merge: observation.allow_auto_merge,
        required_contexts: observation.required_contexts.clone(),
        opt_out_label: args.opt_out_label.clone(),
        max_transient_reruns: args.max_transient_reruns,
    };
    let mut unhealthy = false;
    let mut reports = Vec::new();
    for pr in &observation.prs {
        let attempts = attempts_for(ledger, &observation.repo, &pr.fact);
        let decision = classify_pr(&pr.fact, &policy, &attempts);
        let (mutation, error) = if args.apply {
            mutate_pr(actions, observation, pr, &decision, ledger)
        } else {
            (None, None)
        };
        if error.is_some() {
            unhealthy = true;
        }
        reports.push(PrReport {
            number: pr.fact.number,
            head_sha: pr.fact.head_sha.clone(),
            decision,
            mutation,
            error,
        });
    }
    let mut cancellations = Vec::new();
    if args.coalesce {
        let current_heads = observation
            .prs
            .iter()
            .map(|pr| (pr.fact.head_branch.clone(), pr.fact.head_sha.clone()))
            .collect();
        for cancellation in plan_run_coalescing(
            &observation.runs,
            &current_heads,
            &observation.merge_group_heads,
        ) {
            let (mutation, error) = if args.apply {
                apply_run_cancellation(actions, observation, &cancellation, ledger)
            } else {
                (None, None)
            };
            if error.is_some() {
                unhealthy = true;
            }
            cancellations.push(CancellationReport {
                run_id: cancellation.run_id,
                reason: format!("{:?}", cancellation.reason).to_ascii_lowercase(),
                mutation,
                error,
            });
        }
    }
    (
        RepoReport {
            repo: observation.repo.clone(),
            base: args.base.clone(),
            allow_auto_merge: observation.allow_auto_merge,
            merge_queue: observation.merge_queue,
            merge_path: if observation.merge_queue {
                "native_queue_exact_head".to_owned()
            } else {
                "private_free_exact_head_rest".to_owned()
            },
            required_contexts: observation.required_contexts.clone(),
            prs: reports,
            cancellations,
            errors: Vec::new(),
        },
        unhealthy,
    )
}

fn apply_run_cancellation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let expected_head = observation
        .runs
        .iter()
        .find(|run| run.id == cancellation.run_id)
        .map(|run| run.head_sha.as_str())
        .unwrap_or_default();
    match revalidate_queued_run(
        actions,
        &observation.repo,
        cancellation.run_id,
        expected_head,
    ) {
        Ok(false) => (Some("skipped_after_live_revalidation".to_owned()), None),
        Ok(true) => match actions.cancel_workflow_run(&observation.repo, cancellation.run_id) {
            Ok(()) => {
                record_audit(
                    ledger,
                    &observation.repo,
                    &format!("run:{}", cancellation.run_id),
                    "cancel_revalidated_queued_run",
                );
                (Some("cancelled".to_owned()), None)
            }
            Err(error) => (None, Some(error.to_string())),
        },
        Err(error) => (None, Some(error)),
    }
}

fn revalidate_queued_run(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
    expected_head: &str,
) -> Result<bool, String> {
    if !is_full_sha(expected_head) {
        return Ok(false);
    }
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{repo}/actions/runs/{run_id}"),
        ],
        "queued-run revalidation",
    )?;
    Ok(
        value.get("status").and_then(Value::as_str) == Some("queued")
            && value
                .get("head_sha")
                .and_then(Value::as_str)
                .is_some_and(|head| head.eq_ignore_ascii_case(expected_head)),
    )
}

fn attempts_for(ledger: &StewardLedger, repo: &str, pr: &StewardPullRequest) -> BTreeMap<u64, u32> {
    pr.checks
        .iter()
        .filter_map(|check| {
            let run_id = check.run_id?;
            let key = attempt_key(repo, pr.number, &pr.head_sha, run_id);
            Some((
                run_id,
                ledger.transient_attempts.get(&key).copied().unwrap_or(0),
            ))
        })
        .collect()
}

fn mutate_pr(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    match decision {
        StewardDecision::ArmMergeQueue => {
            let query = "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head}){mergeQueueEntry{position}}}";
            let result = actions.run_gh(&[
                "api".to_owned(),
                "graphql".to_owned(),
                "-f".to_owned(),
                format!("query={query}"),
                "-F".to_owned(),
                format!("id={}", pr.node_id),
                "-F".to_owned(),
                format!("head={}", pr.fact.head_sha),
            ]);
            match result {
                Ok(_) => {
                    record_audit(
                        ledger,
                        &observation.repo,
                        &format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha),
                        "enqueue_exact_head",
                    );
                    (Some("enqueued".to_owned()), None)
                }
                Err(error) => (None, Some(error.to_string())),
            }
        }
        StewardDecision::ExactHeadMerge => {
            let result = gh_json(
                actions,
                &[
                    "api".to_owned(),
                    "--method".to_owned(),
                    "PUT".to_owned(),
                    format!("repos/{}/pulls/{}/merge", observation.repo, pr.fact.number),
                    "-f".to_owned(),
                    format!("sha={}", pr.fact.head_sha),
                    "-f".to_owned(),
                    "merge_method=merge".to_owned(),
                ],
                "exact-head merge",
            );
            match result {
                Ok(value) if value.get("merged").and_then(Value::as_bool) == Some(true) => {
                    record_audit(
                        ledger,
                        &observation.repo,
                        &format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha),
                        "merge_exact_head",
                    );
                    (Some("merged".to_owned()), None)
                }
                Ok(value) => (
                    None,
                    Some(format!(
                        "GitHub refused exact-head merge: {}",
                        value
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown reason")
                    )),
                ),
                Err(error) => (None, Some(error)),
            }
        }
        StewardDecision::RerunTransient { run_ids } => {
            let mut errors = Vec::new();
            let mut rerun = Vec::new();
            for run_id in run_ids {
                match actions.rerun_failed_run(&observation.repo, *run_id) {
                    Ok(()) => {
                        let key = attempt_key(
                            &observation.repo,
                            pr.fact.number,
                            &pr.fact.head_sha,
                            *run_id,
                        );
                        *ledger.transient_attempts.entry(key).or_default() += 1;
                        rerun.push(*run_id);
                        record_audit(
                            ledger,
                            &observation.repo,
                            &format!("run:{run_id}:{}", pr.fact.head_sha),
                            "rerun_transient",
                        );
                    }
                    Err(error) => errors.push(error.to_string()),
                }
            }
            if errors.is_empty() {
                (Some(format!("reran {rerun:?}")), None)
            } else {
                (None, Some(errors.join("; ")))
            }
        }
        _ => (None, None),
    }
}

fn attempt_key(repo: &str, pr: u64, head: &str, run_id: u64) -> String {
    format!("{repo}#{pr}:{head}:{run_id}")
}

fn record_audit(ledger: &mut StewardLedger, repo: &str, subject: &str, action: &str) {
    ledger.audit.push(LedgerAudit {
        at: Utc::now().to_rfc3339(),
        repo: repo.to_owned(),
        subject: subject.to_owned(),
        action: action.to_owned(),
    });
    if ledger.audit.len() > 1_000 {
        ledger.audit.drain(..ledger.audit.len() - 1_000);
    }
}

fn load_ledger(path: &Path) -> Result<StewardLedger, CliFailure> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|error| CliFailure::new(1, format!("invalid steward ledger: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(StewardLedger::default()),
        Err(error) => Err(CliFailure::new(
            1,
            format!("could not read steward ledger {}: {error}", path.display()),
        )),
    }
}

fn save_ledger(path: &Path, ledger: &StewardLedger) -> Result<(), CliFailure> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "could not create steward state {}: {error}",
                    parent.display()
                ),
            )
        })?;
    }
    let payload = serde_json::to_vec_pretty(ledger)
        .map_err(|error| CliFailure::new(1, format!("could not encode steward ledger: {error}")))?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, payload).map_err(|error| {
        CliFailure::new(
            1,
            format!("could not write steward ledger {}: {error}", temp.display()),
        )
    })?;
    fs::rename(&temp, path).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "could not publish steward ledger {}: {error}",
                path.display()
            ),
        )
    })
}

fn render_report<W: Write>(
    stdout: &mut W,
    json_output: bool,
    apply: bool,
    reports: &[RepoReport],
) -> Result<(), CliFailure> {
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("apply".to_owned(), Value::from(apply));
        data.insert(
            "repos".to_owned(),
            serde_json::to_value(reports).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        return write_json_envelope(stdout, "runner.steward", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "merge steward: mode={}",
        if apply { "apply" } else { "dry-run" }
    )
    .map_err(|error| io_failure(&error))?;
    for repo in reports {
        writeln!(
            stdout,
            "{} base={} path={} queue={} native_auto_merge={} required={}",
            repo.repo,
            repo.base,
            repo.merge_path,
            repo.merge_queue,
            repo.allow_auto_merge,
            if repo.required_contexts.is_empty() {
                "all-observed".to_owned()
            } else {
                repo.required_contexts.join(",")
            }
        )
        .map_err(|error| io_failure(&error))?;
        for pr in &repo.prs {
            writeln!(
                stdout,
                "  #{} {} {:?}{}{}",
                pr.number,
                &pr.head_sha[..pr.head_sha.len().min(12)],
                pr.decision,
                pr.mutation
                    .as_ref()
                    .map_or_else(String::new, |value| format!(" mutation={value}")),
                pr.error
                    .as_ref()
                    .map_or_else(String::new, |value| format!(" ERROR={value}"))
            )
            .map_err(|error| io_failure(&error))?;
        }
        for cancellation in &repo.cancellations {
            writeln!(
                stdout,
                "  run {} cancel={}{}{}",
                cancellation.run_id,
                cancellation.reason,
                cancellation
                    .mutation
                    .as_ref()
                    .map_or_else(String::new, |value| format!(" mutation={value}")),
                cancellation
                    .error
                    .as_ref()
                    .map_or_else(String::new, |value| format!(" ERROR={value}"))
            )
            .map_err(|error| io_failure(&error))?;
        }
        for error in &repo.errors {
            writeln!(stdout, "  ERROR: {error}").map_err(|error| io_failure(&error))?;
        }
    }
    Ok(())
}

fn io_failure(error: &std::io::Error) -> CliFailure {
    CliFailure::new(1, error.to_string())
}

fn is_private_free_entitlement(message: &str) -> bool {
    message.contains("Upgrade to GitHub Pro or make this repository public")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_check_rollup_shapes() {
        let check = parse_check(&serde_json::json!({
            "__typename": "CheckRun",
            "name": "macos",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
            "detailsUrl": "https://github.com/o/r/actions/runs/123/job/456"
        }))
        .expect("check");
        assert_eq!(check.run_id, Some(123));
        let context = parse_check(&serde_json::json!({
            "__typename": "StatusContext",
            "context": "freeze",
            "state": "PENDING",
            "targetUrl": "https://github.com/o/r/actions/runs/789"
        }))
        .expect("context");
        assert_eq!(context.status, "IN_PROGRESS");
        assert_eq!(context.run_id, Some(789));
    }

    #[test]
    fn entitlement_match_is_exact_enough_not_to_swallow_generic_forbidden() {
        assert!(is_private_free_entitlement(
            "Upgrade to GitHub Pro or make this repository public to enable this feature."
        ));
        assert!(!is_private_free_entitlement("HTTP 403 forbidden"));
    }
}
