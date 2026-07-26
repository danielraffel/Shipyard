use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CliFailure;
use crate::cloud::GitHubActions;
use crate::identity::RuntimeMode;
use crate::merge_queue_control::{
    MergeQueueMutationGuard, supersede_uncertainty, uncertain_mutations,
};
use crate::merge_steward::{
    QueueFrontPressure, RunCancellation, RunCancellationReason, StewardCheck, StewardDecision,
    StewardJob, StewardPolicy, StewardPullRequest, StewardRun, classify_pr,
    is_capacity_preemption_workflow, is_full_sha, is_safe_capacity_preemption,
    plan_capacity_preemptions, plan_run_coalescing, preemption_key, queue_front_waits_for_pool,
};
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::ship_state::{ShipState, ShipStateStore};

pub(super) struct StewardCommandArgs {
    pub(super) repos: Vec<String>,
    pub(super) base: String,
    pub(super) opt_out_label: String,
    pub(super) max_transient_reruns: u32,
    pub(super) coalesce: bool,
    pub(super) preempt_capacity: bool,
    pub(super) max_preemptions_per_head: u32,
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
    base: String,
    allow_auto_merge: bool,
    merge_queue: bool,
    merge_method: Option<String>,
    required_contexts: Vec<String>,
    prs: Vec<ObservedPr>,
    runs: Vec<StewardRun>,
    merge_group_heads: BTreeMap<u64, String>,
    merge_group_enqueued_at: BTreeMap<u64, String>,
    preemption_error: Option<String>,
}

type MergeQueueSnapshot = (
    bool,
    BTreeMap<u64, u64>,
    BTreeMap<u64, String>,
    BTreeMap<u64, String>,
);

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
    preemption_attempts: BTreeMap<String, u32>,
    #[serde(default)]
    pending_cancellations: BTreeMap<String, PendingCancellation>,
    #[serde(default)]
    audit: Vec<LedgerAudit>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PendingCancellation {
    repo: String,
    base: String,
    run_id: u64,
    workflow_id: u64,
    run_attempt: u64,
    head_sha: String,
    head_branch: String,
    pr_number: u64,
    front_head: String,
    initiated_at: String,
    phase: PendingCancellationPhase,
    mutation_correlation_id: String,
    mutation_kind: PendingMutationKind,
    reason: String,
    opt_out_label: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PendingCancellationPhase {
    Intent,
    Accepted,
    Skipped,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PendingMutationKind {
    NormalCancel,
    ForceCancel,
}

#[derive(Deserialize, Serialize)]
struct LedgerAudit {
    at: String,
    repo: String,
    subject: String,
    action: String,
}

struct NonTerminalRun {
    status: String,
    jobs: Vec<StewardJob>,
}

enum PendingRunState {
    Terminal,
    NonTerminal(NonTerminalRun),
}

struct CapacityRevalidation {
    candidate: StewardRun,
    front_enqueued_at: String,
    front_jobs: Vec<StewardJob>,
    current_pr_head: Option<String>,
}

struct MutationControl {
    store: ShipStateStore,
    cwd: PathBuf,
    mode: RuntimeMode,
    global_dir: PathBuf,
}

struct MutationApplyContext<'a> {
    actions: &'a GitHubActions,
    observation: &'a RepoObservation,
    ledger_path: &'a Path,
    mutation_control: &'a MutationControl,
}

struct CapacityApplyContext<'a> {
    actions: &'a GitHubActions,
    observation: &'a RepoObservation,
    cancellation: &'a RunCancellation,
    ledger_path: &'a Path,
    mutation_control: &'a MutationControl,
}

const CANCEL_TERMINAL_WAIT: Duration = Duration::from_secs(15);
const CANCEL_TERMINAL_POLL: Duration = Duration::from_secs(2);
const PREEMPT_AFTER_SECS: i64 = 900;

pub(super) fn steward_command<W: Write>(
    args: &StewardCommandArgs,
    cwd: &Path,
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repos = resolve_repos(args.repos.clone(), cwd)?;
    let ledger_path = args
        .ledger
        .clone()
        .unwrap_or_else(|| runtime_paths.state_dir.join("merge-steward.json"));
    // Keep this guard alive for the entire apply pass. In particular, it
    // serializes the durable pending-correlation write that precedes mutation
    // guard acquisition, so two steward processes cannot race the shared
    // ledger's temporary file or replace one another's correlation.
    let _ledger_lock = if args.apply {
        Some(acquire_ledger_lock(&ledger_path)?)
    } else {
        None
    };
    let mutation_control = if args.apply {
        Some(MutationControl {
            store: ShipStateStore::new(runtime_paths.state_dir.join("ship")).map_err(|error| {
                CliFailure::new(
                    1,
                    format!("could not open merge-queue mutation state: {error}"),
                )
            })?,
            cwd: cwd.to_path_buf(),
            mode,
            global_dir: runtime_paths.global_dir.clone(),
        })
    } else {
        None
    };
    let mut ledger = load_ledger(&ledger_path)?;
    let recovery_owned_preemption_budget = !ledger.pending_cancellations.is_empty();
    let (mut recovery_errors, mut recovery_cancellations) =
        if let Some(control) = mutation_control.as_ref() {
            resume_pending_cancellations(actions, &ledger_path, &mut ledger, control)
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };
    let mut reports = Vec::new();
    let mut unhealthy = false;
    let mut remaining_preemptions =
        usize::from(!recovery_owned_preemption_budget && ledger.pending_cancellations.is_empty());
    for repo in repos {
        match observe_repo(actions, &repo, &args.base) {
            Ok(observation) => {
                let (mut report, failed, planned_preemptions) = apply_repo_plan(
                    actions,
                    args,
                    &observation,
                    &ledger_path,
                    &mut ledger,
                    remaining_preemptions,
                    mutation_control.as_ref(),
                );
                if let Some(errors) = recovery_errors.remove(&observation.repo) {
                    report.errors.extend(errors);
                }
                if let Some(cancellations) = recovery_cancellations.remove(&observation.repo) {
                    report.cancellations.extend(cancellations);
                }
                remaining_preemptions = remaining_preemptions.saturating_sub(planned_preemptions);
                unhealthy |= failed || !report.errors.is_empty();
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
    append_unmatched_recovery_errors(
        recovery_errors,
        recovery_cancellations,
        &ledger,
        &args.base,
        &mut reports,
        &mut unhealthy,
    );
    if args.apply {
        persist_final_ledger(
            &ledger_path,
            &ledger,
            &args.base,
            &mut reports,
            &mut unhealthy,
        );
    }
    render_report(stdout, json_output, args.apply, &ledger_path, &reports)?;
    Ok(if unhealthy {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn append_unmatched_recovery_errors(
    mut recovery_errors: BTreeMap<String, Vec<String>>,
    mut recovery_cancellations: BTreeMap<String, Vec<CancellationReport>>,
    ledger: &StewardLedger,
    default_base: &str,
    reports: &mut Vec<RepoReport>,
    unhealthy: &mut bool,
) {
    let repos = recovery_errors
        .keys()
        .chain(recovery_cancellations.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for repo in repos {
        let errors = recovery_errors.remove(&repo).unwrap_or_default();
        let cancellations = recovery_cancellations.remove(&repo).unwrap_or_default();
        *unhealthy |= !errors.is_empty();
        let base = ledger
            .pending_cancellations
            .values()
            .find(|pending| pending.repo == repo)
            .map_or_else(|| default_base.to_owned(), |pending| pending.base.clone());
        reports.push(RepoReport {
            repo,
            base,
            allow_auto_merge: false,
            merge_queue: false,
            merge_path: "pending_cancellation_recovery".to_owned(),
            required_contexts: Vec::new(),
            prs: Vec::new(),
            cancellations,
            errors,
        });
    }
}

fn persist_final_ledger(
    ledger_path: &Path,
    ledger: &StewardLedger,
    base: &str,
    reports: &mut Vec<RepoReport>,
    unhealthy: &mut bool,
) {
    let Err(error) = save_ledger(ledger_path, ledger) else {
        return;
    };
    *unhealthy = true;
    let message = format!("final steward ledger persistence failed: {}", error.message);
    if let Some(report) = reports.first_mut() {
        report.errors.push(message);
    } else {
        reports.push(RepoReport {
            repo: "steward".to_owned(),
            base: base.to_owned(),
            allow_auto_merge: false,
            merge_queue: false,
            merge_path: "unreadable".to_owned(),
            required_contexts: Vec::new(),
            prs: Vec::new(),
            cancellations: Vec::new(),
            errors: vec![message],
        });
    }
}

fn acquire_ledger_lock(path: &Path) -> Result<fs::File, CliFailure> {
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
    let lock_path = path.with_extension("json.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "could not open steward lock {}: {error}",
                    lock_path.display()
                ),
            )
        })?;
    file.try_lock_exclusive().map_err(|error| {
        let reason = if error.kind() == std::io::ErrorKind::WouldBlock {
            "another steward apply pass is already running".to_owned()
        } else {
            error.to_string()
        };
        CliFailure::new(
            1,
            format!(
                "could not lock steward state {}: {reason}",
                lock_path.display()
            ),
        )
    })?;
    Ok(file)
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
    let repo = canonical_repo_name(&settings)?;
    let allow_auto_merge = settings
        .get("allow_auto_merge")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let merge_method = if settings
        .get("allow_merge_commit")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Some("merge".to_owned())
    } else if settings
        .get("allow_squash_merge")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Some("squash".to_owned())
    } else if settings
        .get("allow_rebase_merge")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Some("rebase".to_owned())
    } else {
        None
    };
    let required_contexts = required_contexts(actions, &repo, base)?;
    let (merge_queue, queue_positions, merge_group_heads, merge_group_enqueued_at) =
        merge_queue_snapshot(actions, &repo, base)?;
    let prs = pull_requests(actions, &repo, base, &queue_positions)?;
    let mut runs = active_runs(actions, &repo)?;
    let front_head = queue_positions
        .iter()
        .min_by_key(|(_, position)| **position)
        .and_then(|(number, _)| merge_group_heads.get(number))
        .map(String::as_str);
    let preemption_error = hydrate_preemption_jobs(actions, &repo, front_head, &mut runs).err();
    Ok(RepoObservation {
        repo,
        base: base.to_owned(),
        allow_auto_merge,
        merge_queue,
        merge_method,
        required_contexts,
        prs,
        runs,
        merge_group_heads,
        merge_group_enqueued_at,
        preemption_error,
    })
}

fn canonical_repo_name(settings: &Value) -> Result<String, String> {
    let full_name = settings
        .get("full_name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "repository settings missing canonical full_name".to_owned())?;
    let Some((owner, name)) = full_name.split_once('/') else {
        return Err("repository settings returned malformed canonical full_name".to_owned());
    };
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err("repository settings returned malformed canonical full_name".to_owned());
    }
    Ok(full_name.to_owned())
}

fn gh_json(actions: &GitHubActions, args: &[String], purpose: &str) -> Result<Value, String> {
    let raw = actions
        .run_gh(args)
        .map_err(|error| format!("{purpose} failed: {error}"))?;
    serde_json::from_str(&raw)
        .map_err(|error| format!("{purpose} returned malformed JSON: {error}"))
}

fn gh_json_timeout(
    actions: &GitHubActions,
    args: &[String],
    purpose: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let raw = actions
        .run_gh_with_timeout(args, timeout)
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
        Err(error)
            if is_private_free_entitlement(&error.to_string())
                || is_admin_protection_denied(&error.to_string()) =>
        {
            return Ok(Vec::new());
        }
        Err(error) if error.to_string().contains("HTTP 404") => {
            return required_contexts_from_evaluated_rules(actions, repo, base);
        }
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
    contexts.extend(required_contexts_from_evaluated_rules(actions, repo, base)?);
    contexts.sort();
    contexts.dedup();
    Ok(contexts)
}

fn required_contexts_from_evaluated_rules(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Vec<String>, String> {
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{repo}/rules/branches/{base}"),
            "--paginate".to_owned(),
            "--slurp".to_owned(),
        ],
        "evaluated branch rules",
    )?;
    evaluated_required_contexts(&value)
}

fn evaluated_required_contexts(value: &Value) -> Result<Vec<String>, String> {
    let pages = value
        .as_array()
        .ok_or_else(|| "evaluated branch rules was not an array".to_owned())?;
    let rules = if pages.iter().all(Value::is_array) {
        pages
            .iter()
            .flat_map(|page| page.as_array().into_iter().flatten())
            .collect::<Vec<_>>()
    } else {
        pages.iter().collect()
    };
    let mut contexts = rules
        .iter()
        .filter(|rule| rule.get("type").and_then(Value::as_str) == Some("required_status_checks"))
        .flat_map(|rule| {
            rule.pointer("/parameters/required_status_checks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|check| check.get("context").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<Vec<_>>();
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
    let query = "query($owner:String!,$name:String!,$branch:String!){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){entries(first:100){nodes{position enqueuedAt headCommit{oid} pullRequest{number}} pageInfo{hasNextPage}}}}}";
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
            return Ok((false, BTreeMap::new(), BTreeMap::new(), BTreeMap::new()));
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
            return Ok((false, BTreeMap::new(), BTreeMap::new(), BTreeMap::new()));
        }
        return Err(format!("merge-queue GraphQL errors: {text}"));
    }
    let Some(queue) = value.pointer("/data/repository/mergeQueue") else {
        return Err("merge-queue response missing repository.mergeQueue".to_owned());
    };
    if queue.is_null() {
        return Ok((false, BTreeMap::new(), BTreeMap::new(), BTreeMap::new()));
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
    let mut enqueued = BTreeMap::new();
    for node in nodes {
        let Some(number) = node.pointer("/pullRequest/number").and_then(Value::as_u64) else {
            return Err("merge-queue entry missing PR number".to_owned());
        };
        let Some(position) = node.get("position").and_then(Value::as_u64) else {
            return Err(format!("merge-queue PR #{number} missing position"));
        };
        positions.insert(number, position);
        let Some(enqueued_at) = node.get("enqueuedAt").and_then(Value::as_str) else {
            return Err(format!("merge-queue PR #{number} missing enqueuedAt"));
        };
        enqueued.insert(number, enqueued_at.to_owned());
        if let Some(head) = node
            .pointer("/headCommit/oid")
            .and_then(Value::as_str)
            .filter(|head| is_full_sha(head))
        {
            heads.insert(number, head.to_owned());
        }
    }
    Ok((true, positions, heads, enqueued))
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
    for status in ["queued", "waiting", "pending", "requested", "in_progress"] {
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
    all.sort_unstable_by_key(|run| run.id);
    all.dedup_by_key(|run| run.id);
    Ok(all)
}

fn parse_run(value: &Value) -> Option<StewardRun> {
    let pull_request_number = value
        .get("pull_requests")
        .and_then(Value::as_array)
        .filter(|pull_requests| pull_requests.len() == 1)
        .and_then(|pull_requests| pull_requests[0].get("number"))
        .and_then(Value::as_u64);
    Some(StewardRun {
        id: value.get("id")?.as_u64()?,
        workflow_id: value.get("workflow_id")?.as_u64()?,
        run_attempt: value
            .get("run_attempt")
            .and_then(Value::as_u64)
            .unwrap_or(1),
        workflow: value.get("name")?.as_str()?.to_owned(),
        head_sha: value.get("head_sha")?.as_str()?.to_owned(),
        head_branch: value.get("head_branch")?.as_str()?.to_owned(),
        status: value.get("status")?.as_str()?.to_owned(),
        event: value.get("event")?.as_str()?.to_owned(),
        pull_request_number,
        created_at: value.get("created_at")?.as_str()?.to_owned(),
        jobs: Vec::new(),
    })
}

fn hydrate_preemption_jobs(
    actions: &GitHubActions,
    repo: &str,
    front_head: Option<&str>,
    runs: &mut [StewardRun],
) -> Result<(), String> {
    let Some(front_head) = front_head else {
        return Ok(());
    };
    for run in runs {
        let is_front_candidate =
            run.event == "merge_group" && front_head.eq_ignore_ascii_case(&run.head_sha);
        let is_preemption_candidate = run.event == "pull_request"
            && run.status.eq_ignore_ascii_case("in_progress")
            && is_capacity_preemption_workflow(&run.workflow);
        if is_front_candidate || is_preemption_candidate {
            run.jobs = fetch_run_jobs(actions, repo, run.id)?;
        }
    }
    Ok(())
}

fn fetch_run_jobs(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
) -> Result<Vec<StewardJob>, String> {
    let mut all = Vec::new();
    for page in 1..=10 {
        let value = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{repo}/actions/runs/{run_id}/jobs?filter=all&per_page=100&page={page}"
                ),
            ],
            "workflow jobs",
        )?;
        let rows = value
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("workflow run {run_id} response missing jobs"))?;
        let count = rows.len();
        for row in rows {
            all.push(parse_job(row)?);
        }
        if count < 100 {
            return Ok(all);
        }
    }
    Err(format!(
        "workflow run {run_id} exceeds 1000 jobs; refusing partial scan"
    ))
}

fn fetch_run_jobs_before(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
    deadline: Instant,
) -> Result<Vec<StewardJob>, String> {
    let mut all = Vec::new();
    for page in 1..=10 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("workflow run {run_id} job fetch timed out"));
        }
        let value = gh_json_timeout(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{repo}/actions/runs/{run_id}/jobs?filter=all&per_page=100&page={page}"
                ),
            ],
            "workflow jobs",
            remaining,
        )?;
        let rows = value
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("workflow run {run_id} response missing jobs"))?;
        let count = rows.len();
        for row in rows {
            all.push(parse_job(row)?);
        }
        if count < 100 {
            return Ok(all);
        }
    }
    Err(format!(
        "workflow run {run_id} exceeds 1000 jobs; refusing partial scan"
    ))
}

fn parse_job(value: &Value) -> Result<StewardJob, String> {
    Ok(StewardJob {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "workflow job missing name".to_owned())?
            .to_owned(),
        status: value
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| "workflow job missing status".to_owned())?
            .to_owned(),
        conclusion: value
            .get("conclusion")
            .and_then(Value::as_str)
            .map(str::to_owned),
        labels: value
            .get("labels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        runner_name: value
            .get("runner_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
    })
}

fn apply_repo_plan(
    actions: &GitHubActions,
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    remaining_preemptions: usize,
    mutation_control: Option<&MutationControl>,
) -> (RepoReport, bool, usize) {
    let policy = StewardPolicy {
        merge_queue: observation.merge_queue,
        native_auto_merge: observation.allow_auto_merge,
        required_contexts: observation.required_contexts.clone(),
        opt_out_label: args.opt_out_label.clone(),
        max_transient_reruns: args.max_transient_reruns,
    };
    let (reports, pr_mutation_failed) = apply_pr_plans(
        actions,
        args,
        observation,
        &policy,
        ledger_path,
        ledger,
        mutation_control,
    );
    let mut unhealthy = observation.preemption_error.is_some() || pr_mutation_failed;
    let mut planned_cancellations = Vec::new();
    if args.coalesce {
        let current_heads = current_pull_request_heads(&observation.prs);
        let opted_out = opted_out_pull_requests(&observation.prs, &args.opt_out_label);
        planned_cancellations.extend(plan_run_coalescing(
            &observation.runs,
            &current_heads,
            &observation.merge_group_heads,
            &opted_out,
        ));
    }
    planned_cancellations.extend(plan_repo_capacity_preemptions(
        args,
        observation,
        ledger,
        remaining_preemptions,
    ));
    let capacity_preemptions_planned = planned_cancellations
        .iter()
        .filter(|cancellation| {
            matches!(
                cancellation.reason,
                RunCancellationReason::AdvisoryPreambleCapacityTheft
                    | RunCancellationReason::LowerPriorityBranchPreamble
            )
        })
        .count();
    let mut cancellations = Vec::new();
    for cancellation in planned_cancellations {
        let (mutation, error) = if args.apply {
            apply_run_cancellation(
                actions,
                observation,
                &cancellation,
                &args.opt_out_label,
                ledger_path,
                ledger,
                mutation_control.expect("apply mode requires mutation control"),
            )
        } else {
            (None, None)
        };
        if error.is_some() {
            unhealthy = true;
        }
        cancellations.push(CancellationReport {
            run_id: cancellation.run_id,
            reason: cancellation_reason_label(cancellation.reason),
            mutation,
            error,
        });
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
            errors: observation.preemption_error.iter().cloned().collect(),
        },
        unhealthy,
        capacity_preemptions_planned,
    )
}

fn apply_pr_plans(
    actions: &GitHubActions,
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: Option<&MutationControl>,
) -> (Vec<PrReport>, bool) {
    let mut unhealthy = false;
    let mutation_context = mutation_control.map(|mutation_control| MutationApplyContext {
        actions,
        observation,
        ledger_path,
        mutation_control,
    });
    let reports = observation
        .prs
        .iter()
        .map(|pr| {
            let attempts = attempts_for(ledger, &observation.repo, &pr.fact);
            let decision = classify_pr(&pr.fact, policy, &attempts);
            let (mutation, error) = if args.apply {
                mutate_pr(
                    mutation_context
                        .as_ref()
                        .expect("apply mode requires mutation control"),
                    pr,
                    policy,
                    &decision,
                    ledger,
                )
            } else {
                (None, None)
            };
            unhealthy |= error.is_some();
            PrReport {
                number: pr.fact.number,
                head_sha: pr.fact.head_sha.clone(),
                decision,
                mutation,
                error,
            }
        })
        .collect();
    (reports, unhealthy)
}

fn plan_repo_capacity_preemptions(
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    ledger: &StewardLedger,
    remaining_preemptions: usize,
) -> Vec<RunCancellation> {
    if !args.preempt_capacity
        || args.max_preemptions_per_head == 0
        || observation.preemption_error.is_some()
    {
        return Vec::new();
    }
    let Some(pressure) = queue_front_pressure(observation) else {
        return Vec::new();
    };
    let prefix = format!("{}:", observation.repo);
    let attempted = ledger
        .preemption_attempts
        .iter()
        .filter(|(_, count)| **count >= args.max_preemptions_per_head)
        .filter_map(|(key, _)| key.strip_prefix(&prefix).map(str::to_owned))
        .collect();
    let current_heads = current_pull_request_heads(&observation.prs);
    let opted_out = opted_out_pull_requests(&observation.prs, &args.opt_out_label);
    plan_capacity_preemptions(
        &observation.runs,
        &current_heads,
        &opted_out,
        &pressure,
        &attempted,
        remaining_preemptions,
    )
}

fn cancellation_reason_label(reason: RunCancellationReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{reason:?}").to_ascii_lowercase())
}

fn queue_front_pressure(observation: &RepoObservation) -> Option<QueueFrontPressure> {
    let front = queue_front_pr(observation)?;
    let head_sha = observation
        .merge_group_heads
        .get(&front.fact.number)?
        .to_owned();
    let enqueued_at = observation
        .merge_group_enqueued_at
        .get(&front.fact.number)?;
    Some(QueueFrontPressure {
        head_sha,
        old_enough: timestamp_old_enough(enqueued_at),
    })
}

fn queue_front_head(observation: &RepoObservation) -> Option<&str> {
    let front = queue_front_pr(observation)?;
    observation
        .merge_group_heads
        .get(&front.fact.number)
        .map(String::as_str)
}

fn queue_front_pr(observation: &RepoObservation) -> Option<&ObservedPr> {
    Some(
        observation
            .prs
            .iter()
            .filter_map(|pr| pr.fact.queue_position.map(|position| (position, pr)))
            .min_by_key(|(position, _)| *position)?
            .1,
    )
}

fn timestamp_old_enough(timestamp: &str) -> bool {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .is_some_and(|created| {
            (Utc::now() - created.with_timezone(&Utc)).num_seconds() >= PREEMPT_AFTER_SECS
        })
}

fn apply_run_cancellation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    opt_out_label: &str,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
) -> (Option<String>, Option<String>) {
    if matches!(
        cancellation.reason,
        RunCancellationReason::AdvisoryPreambleCapacityTheft
            | RunCancellationReason::LowerPriorityBranchPreamble
    ) {
        return apply_capacity_preemption(
            &CapacityApplyContext {
                actions,
                observation,
                cancellation,
                ledger_path,
                mutation_control,
            },
            opt_out_label,
            ledger,
        );
    }
    let Some(observed) = observation
        .runs
        .iter()
        .find(|run| run.id == cancellation.run_id)
    else {
        return (None, Some("planned run observation disappeared".to_owned()));
    };
    match revalidate_coalescing_cancellation(
        actions,
        observation,
        observed,
        cancellation,
        opt_out_label,
    ) {
        Ok(false) => (Some("skipped_after_live_revalidation".to_owned()), None),
        Ok(true) => {
            let guard = match acquire_run_mutation_guard(
                mutation_control,
                observation,
                observed,
                &format!("runner steward cancel run {}", cancellation.run_id),
            ) {
                Ok(guard) => guard,
                Err(error) => return (None, Some(error)),
            };
            match actions.cancel_workflow_run(&observation.repo, cancellation.run_id) {
                Ok(()) => {
                    if let Err(error) = guard.finish("cancel_accepted") {
                        return (
                            Some("cancelled".to_owned()),
                            Some(format!(
                                "cancel accepted but mutation audit failed: {error}"
                            )),
                        );
                    }
                    record_audit(
                        ledger,
                        &observation.repo,
                        &format!("run:{}", cancellation.run_id),
                        "cancel_revalidated_queued_run",
                    );
                    (Some("cancelled".to_owned()), None)
                }
                Err(error) => (None, Some(error.to_string())),
            }
        }
        Err(error) => (None, Some(error)),
    }
}

fn apply_capacity_preemption(
    context: &CapacityApplyContext<'_>,
    opt_out_label: &str,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let Some(observed) = context
        .observation
        .runs
        .iter()
        .find(|run| run.id == context.cancellation.run_id)
    else {
        return (None, Some("planned run observation disappeared".to_owned()));
    };
    let Some(expected_front) = queue_front_head(context.observation) else {
        return (Some("skipped_after_front_revalidation".to_owned()), None);
    };
    let (guard, cancel_live, pending) =
        match prepare_capacity_preemption(context, opt_out_label, ledger, observed, expected_front)
        {
            Ok(Some(prepared)) => prepared,
            Ok(None) => {
                return (
                    Some("skipped_after_precancel_revalidation".to_owned()),
                    None,
                );
            }
            Err(error) => return (None, Some(error)),
        };
    match current_pending_run_identity_matches(context.actions, &pending) {
        Ok(true) => {}
        Ok(false) => {
            let key = pending_cancellation_key(&pending);
            if let Err(error) = mark_cancellation_skipped(ledger, context.ledger_path, &key) {
                return (None, Some(error));
            }
            if let Err(error) = guard.finish("skipped_after_attempt_revalidation") {
                return (None, Some(format!("mutation audit failed: {error}")));
            }
            if let Err(error) = clear_pending_cancellation(
                ledger,
                context.ledger_path,
                &key,
                &pending,
                "skipped_after_attempt_revalidation",
            ) {
                return (None, Some(error));
            }
            return (Some("skipped_after_attempt_revalidation".to_owned()), None);
        }
        Err(error) => {
            let audit_error = guard.finish("attempt_revalidation_failed").err();
            return (
                None,
                Some(format!(
                    "{error}{}",
                    audit_error.map_or_else(String::new, |audit_error| format!(
                        "; mutation audit also failed: {audit_error}"
                    ))
                )),
            );
        }
    }
    match context
        .actions
        .cancel_workflow_run(&context.observation.repo, context.cancellation.run_id)
    {
        Ok(()) => {
            if let Err(error) =
                mark_cancellation_accepted(context, expected_front, &cancel_live.candidate, ledger)
            {
                return (
                    Some("cancelled_after_job_revalidation".to_owned()),
                    Some(format!(
                        "cancel accepted but pending recovery persistence failed: {error}"
                    )),
                );
            }
            if let Err(error) = guard.finish("cancel_accepted") {
                return (
                    Some("cancelled_after_job_revalidation".to_owned()),
                    Some(format!(
                        "cancel accepted but mutation audit failed: {error}"
                    )),
                );
            }
            complete_capacity_cancellation(context, expected_front, &cancel_live.candidate, ledger)
        }
        Err(error) => (None, Some(error.to_string())),
    }
}

fn prepare_capacity_preemption(
    context: &CapacityApplyContext<'_>,
    opt_out_label: &str,
    ledger: &mut StewardLedger,
    observed: &StewardRun,
    expected_front: &str,
) -> Result<
    Option<(
        MergeQueueMutationGuard,
        CapacityRevalidation,
        PendingCancellation,
    )>,
    String,
> {
    let (guard, pending) =
        start_capacity_preemption(context, opt_out_label, ledger, observed, expected_front)?;
    let cancel_live = match revalidate_capacity_preemption(
        context.actions,
        context.observation,
        context.cancellation,
        observed,
        expected_front,
        opt_out_label,
    ) {
        Ok(Some(evidence)) => evidence,
        Ok(None) => {
            mark_cancellation_skipped(
                ledger,
                context.ledger_path,
                &pending_cancellation_key(&pending),
            )?;
            guard
                .finish("skipped_after_precancel_revalidation")
                .map_err(|error| format!("mutation audit failed: {error}"))?;
            clear_pending_cancellation(
                ledger,
                context.ledger_path,
                &pending_cancellation_key(&pending),
                &pending,
                "skipped_after_precancel_revalidation",
            )?;
            return Ok(None);
        }
        Err(error) => {
            let audit_error = guard.finish("revalidation_failed").err();
            return Err(format!(
                "{error}{}",
                audit_error.map_or_else(String::new, |error| format!(
                    "; mutation audit also failed: {error}"
                ))
            ));
        }
    };
    if let Err(error) = persist_capacity_evidence(
        context.observation,
        context.cancellation,
        expected_front,
        &cancel_live,
        context.ledger_path,
        ledger,
    ) {
        let audit_error = guard.finish("evidence_persistence_failed").err();
        return Err(format!(
            "{error}{}",
            audit_error.map_or_else(String::new, |error| format!(
                "; mutation audit also failed: {error}"
            ))
        ));
    }
    Ok(Some((guard, cancel_live, pending)))
}

fn start_capacity_preemption(
    context: &CapacityApplyContext<'_>,
    opt_out_label: &str,
    ledger: &mut StewardLedger,
    observed: &StewardRun,
    expected_front: &str,
) -> Result<(MergeQueueMutationGuard, PendingCancellation), String> {
    let correlation_id = MergeQueueMutationGuard::new_correlation_id();
    let pending = pending_cancellation(
        context,
        expected_front,
        observed,
        &correlation_id,
        PendingCancellationPhase::Intent,
        opt_out_label,
    )?;
    validate_pending_cancellation_authority(context.mutation_control, &pending)?;
    let key = format!("{}:{}", context.observation.repo, preemption_key(observed));
    *ledger.preemption_attempts.entry(key).or_default() += 1;
    record_audit(
        ledger,
        &context.observation.repo,
        &format!(
            "front:{expected_front}:capacity-run:{}:{}",
            context.cancellation.run_id, observed.head_sha
        ),
        &format!(
            "capacity_preemption_started:{:?}",
            context.cancellation.reason
        ),
    );
    persist_pending_cancellation(context.ledger_path, ledger, pending.clone())?;
    let guard = acquire_pending_cancellation_guard_with_correlation(
        context.mutation_control,
        &pending,
        &format!(
            "runner steward preempt capacity run {}",
            context.cancellation.run_id
        ),
        &correlation_id,
    )?;
    Ok((guard, pending))
}

fn persist_pending_cancellation(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    pending: PendingCancellation,
) -> Result<(), String> {
    let key = pending_cancellation_key(&pending);
    let repo = pending.repo.clone();
    let run_id = pending.run_id;
    let phase = pending.phase;
    ledger.pending_cancellations.insert(key, pending);
    record_audit(
        ledger,
        &repo,
        &format!("capacity-run:{run_id}"),
        &format!("capacity_preemption_pending:{phase:?}"),
    );
    save_ledger(ledger_path, ledger).map_err(|error| error.message)
}

fn mark_cancellation_accepted(
    context: &CapacityApplyContext<'_>,
    expected_front: &str,
    run: &StewardRun,
    ledger: &mut StewardLedger,
) -> Result<(), String> {
    let probe = pending_cancellation_key_parts(
        &context.observation.repo,
        context.cancellation.run_id,
        &run.head_sha,
        expected_front,
    );
    let pending = ledger
        .pending_cancellations
        .get_mut(&probe)
        .ok_or_else(|| "pending cancellation intent disappeared".to_owned())?;
    pending.phase = PendingCancellationPhase::Accepted;
    record_audit(
        ledger,
        &context.observation.repo,
        &format!("capacity-run:{}", context.cancellation.run_id),
        "capacity_preemption_pending_after_acceptance",
    );
    save_ledger(context.ledger_path, ledger).map_err(|error| error.message)
}

fn mark_cancellation_skipped(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    key: &str,
) -> Result<(), String> {
    let pending = ledger
        .pending_cancellations
        .get_mut(key)
        .ok_or_else(|| "pending cancellation intent disappeared".to_owned())?;
    pending.phase = PendingCancellationPhase::Skipped;
    let repo = pending.repo.clone();
    let run_id = pending.run_id;
    record_audit(
        ledger,
        &repo,
        &format!("capacity-run:{run_id}"),
        "capacity_preemption_skipped_before_mutation",
    );
    save_ledger(ledger_path, ledger).map_err(|error| error.message)
}

fn pending_cancellation(
    context: &CapacityApplyContext<'_>,
    front_head: &str,
    run: &StewardRun,
    correlation_id: &str,
    phase: PendingCancellationPhase,
    opt_out_label: &str,
) -> Result<PendingCancellation, String> {
    let pr_number = run
        .pull_request_number
        .or_else(|| merge_group_pr_number(run))
        .ok_or_else(|| {
            format!(
                "workflow run {} has no pull-request identity; refusing an unaudited cancellation",
                run.id
            )
        })?;
    Ok(PendingCancellation {
        repo: context.observation.repo.clone(),
        base: context.observation.base.clone(),
        run_id: run.id,
        workflow_id: run.workflow_id,
        run_attempt: run.run_attempt,
        head_sha: run.head_sha.clone(),
        head_branch: run.head_branch.clone(),
        pr_number,
        front_head: front_head.to_owned(),
        initiated_at: Utc::now().to_rfc3339(),
        phase,
        mutation_correlation_id: correlation_id.to_owned(),
        mutation_kind: PendingMutationKind::NormalCancel,
        reason: cancellation_reason_label(context.cancellation.reason),
        opt_out_label: opt_out_label.to_owned(),
    })
}

fn pending_cancellation_key(pending: &PendingCancellation) -> String {
    pending_cancellation_key_parts(
        &pending.repo,
        pending.run_id,
        &pending.head_sha,
        &pending.front_head,
    )
}

fn pending_cancellation_key_parts(repo: &str, run_id: u64, head: &str, front: &str) -> String {
    format!("{repo}#{run_id}:{head}:{front}")
}

fn persist_capacity_evidence(
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    expected_front: &str,
    evidence: &CapacityRevalidation,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
) -> Result<(), String> {
    let final_evidence = serde_json::json!({
        "front_head": expected_front,
        "front_enqueued_at": evidence.front_enqueued_at,
        "front_jobs": evidence.front_jobs,
        "candidate_run": evidence.candidate.id,
        "candidate_head": evidence.candidate.head_sha,
        "candidate_jobs": evidence.candidate.jobs,
        "current_pr_head": evidence.current_pr_head,
    });
    record_audit(
        ledger,
        &observation.repo,
        &format!("capacity-run:{}", cancellation.run_id),
        &format!("capacity_preemption_precancel_evidence:{final_evidence}"),
    );
    save_ledger(ledger_path, ledger).map_err(|error| {
        format!(
            "could not persist final preemption evidence: {}",
            error.message
        )
    })
}

fn resume_pending_cancellations(
    actions: &GitHubActions,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
) -> (
    BTreeMap<String, Vec<String>>,
    BTreeMap<String, Vec<CancellationReport>>,
) {
    let pending = ledger
        .pending_cancellations
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    let mut errors = BTreeMap::<String, Vec<String>>::new();
    let mut cancellations = BTreeMap::<String, Vec<CancellationReport>>::new();
    for (key, cancellation) in pending {
        match resume_pending_cancellation(
            actions,
            ledger_path,
            ledger,
            mutation_control,
            &key,
            &cancellation,
        ) {
            Ok(mutation) => cancellations
                .entry(cancellation.repo.clone())
                .or_default()
                .push(CancellationReport {
                    run_id: cancellation.run_id,
                    reason: cancellation.reason.clone(),
                    mutation: Some(mutation),
                    error: None,
                }),
            Err(error) => {
                record_audit(
                    ledger,
                    &cancellation.repo,
                    &format!("capacity-run:{}", cancellation.run_id),
                    "pending_cancellation_recovery_unhealthy",
                );
                let persistence = save_ledger(ledger_path, ledger).err();
                errors
                    .entry(cancellation.repo.clone())
                    .or_default()
                    .push(format!(
                        "pending cancellation recovery for run {} failed: {error}{}",
                        cancellation.run_id,
                        persistence.map_or_else(String::new, |save_error| format!(
                            "; recovery audit persistence also failed: {}",
                            save_error.message
                        ))
                    ));
            }
        }
    }
    (errors, cancellations)
}

fn resume_pending_cancellation(
    actions: &GitHubActions,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
    key: &str,
    pending: &PendingCancellation,
) -> Result<String, String> {
    if pending.phase == PendingCancellationPhase::Skipped {
        return clear_recovered_skipped_cancellation(
            ledger,
            ledger_path,
            mutation_control,
            key,
            pending,
        );
    }
    match read_pending_run(actions, pending)? {
        PendingRunState::Terminal => {
            supersede_pending_uncertainty(mutation_control, pending)?;
            clear_pending_cancellation(
                ledger,
                ledger_path,
                key,
                pending,
                "pending_cancellation_observed_terminal",
            )?;
            Ok("recovered_terminal".to_owned())
        }
        PendingRunState::NonTerminal(_active)
            if pending.phase == PendingCancellationPhase::Intent =>
        {
            resume_pending_intent(actions, ledger_path, ledger, mutation_control, key, pending)
        }
        PendingRunState::NonTerminal(_active) => {
            supersede_pending_uncertainty(mutation_control, pending)?;
            let Some(active) = wait_for_pending_normal_terminalization(actions, pending)? else {
                clear_pending_cancellation(
                    ledger,
                    ledger_path,
                    key,
                    pending,
                    "pending_normal_cancel_terminalized",
                )?;
                return Ok("recovered_normal_cancel_terminal".to_owned());
            };
            let targets = active_runner_targets(&active.jobs);
            persist_force_cancel_intent(
                ledger,
                ledger_path,
                &pending.repo,
                pending.run_id,
                &active.status,
                &targets,
            )
            .map_err(|error| error.message)?;
            let correlation_id = MergeQueueMutationGuard::new_correlation_id();
            persist_pending_mutation_correlation(
                ledger,
                ledger_path,
                key,
                &correlation_id,
                PendingMutationKind::ForceCancel,
                "pending_force_cancel_intent",
            )?;
            let guard = acquire_pending_cancellation_guard_with_correlation(
                mutation_control,
                pending,
                &format!("runner steward resume force-cancel run {}", pending.run_id),
                &correlation_id,
            )?;
            read_current_pending_run_identity(actions, pending)?;
            actions
                .force_cancel_workflow_run(&pending.repo, pending.run_id)
                .map_err(|error| format!("exact force-cancel failed: {error}"))?;
            guard
                .finish("force_cancel_accepted")
                .map_err(|error| format!("force-cancel mutation audit failed: {error}"))?;
            record_audit(
                ledger,
                &pending.repo,
                &format!("capacity-run:{}", pending.run_id),
                "pending_force_cancel_accepted",
            );
            save_ledger(ledger_path, ledger).map_err(|error| {
                format!(
                    "force-cancel accepted but recovery audit persistence failed: {}",
                    error.message
                )
            })?;
            match wait_for_pending_terminalization(actions, pending)? {
                None => clear_pending_cancellation(
                    ledger,
                    ledger_path,
                    key,
                    pending,
                    "pending_force_cancel_terminalized",
                )
                .map(|()| "recovered_force_cancel_terminal".to_owned()),
                Some(still_active) => Err(format!(
                    "exact force-cancel accepted but run remains {} with active={}",
                    still_active.status,
                    active_runner_targets(&still_active.jobs)
                )),
            }
        }
    }
}

fn clear_recovered_skipped_cancellation(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    mutation_control: &MutationControl,
    key: &str,
    pending: &PendingCancellation,
) -> Result<String, String> {
    supersede_pending_uncertainty(mutation_control, pending)?;
    clear_pending_cancellation(
        ledger,
        ledger_path,
        key,
        pending,
        "pending_skipped_cancellation_cleared",
    )?;
    Ok("recovered_skipped_cancellation".to_owned())
}

fn wait_for_pending_terminalization(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<Option<NonTerminalRun>, String> {
    let deadline = Instant::now() + CANCEL_TERMINAL_WAIT;
    loop {
        match read_pending_run(actions, pending)? {
            PendingRunState::Terminal => return Ok(None),
            PendingRunState::NonTerminal(active)
                if Instant::now() + CANCEL_TERMINAL_POLL >= deadline =>
            {
                return Ok(Some(active));
            }
            PendingRunState::NonTerminal(_) => {
                thread::sleep(
                    CANCEL_TERMINAL_POLL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    }
}

fn wait_for_pending_normal_terminalization(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<Option<NonTerminalRun>, String> {
    let elapsed = DateTime::parse_from_rfc3339(&pending.initiated_at)
        .ok()
        .and_then(|started| (Utc::now() - started.with_timezone(&Utc)).to_std().ok())
        .unwrap_or(CANCEL_TERMINAL_WAIT);
    if elapsed >= CANCEL_TERMINAL_WAIT {
        return match read_pending_run(actions, pending)? {
            PendingRunState::Terminal => Ok(None),
            PendingRunState::NonTerminal(active) => Ok(Some(active)),
        };
    }
    let deadline = Instant::now()
        + CANCEL_TERMINAL_WAIT
            .checked_sub(elapsed)
            .expect("elapsed was checked against cancellation wait");
    loop {
        match read_pending_run(actions, pending)? {
            PendingRunState::Terminal => return Ok(None),
            PendingRunState::NonTerminal(active)
                if Instant::now() + CANCEL_TERMINAL_POLL >= deadline =>
            {
                return Ok(Some(active));
            }
            PendingRunState::NonTerminal(_) => thread::sleep(
                CANCEL_TERMINAL_POLL.min(deadline.saturating_duration_since(Instant::now())),
            ),
        }
    }
}

fn persist_pending_mutation_correlation(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    key: &str,
    correlation_id: &str,
    mutation_kind: PendingMutationKind,
    action: &str,
) -> Result<(), String> {
    let pending = ledger
        .pending_cancellations
        .get_mut(key)
        .ok_or_else(|| "pending cancellation record disappeared".to_owned())?;
    correlation_id.clone_into(&mut pending.mutation_correlation_id);
    pending.mutation_kind = mutation_kind;
    let repo = pending.repo.clone();
    let run_id = pending.run_id;
    record_audit(ledger, &repo, &format!("capacity-run:{run_id}"), action);
    save_ledger(ledger_path, ledger).map_err(|error| {
        format!(
            "could not persist pending mutation correlation: {}",
            error.message
        )
    })
}

fn resume_pending_intent(
    actions: &GitHubActions,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
    key: &str,
    pending: &PendingCancellation,
) -> Result<String, String> {
    let was_uncertain = pending_uncertainty(mutation_control, pending)?;
    let observation = observe_repo(actions, &pending.repo, &pending.base)?;
    let observed = observation
        .runs
        .iter()
        .find(|run| run.id == pending.run_id)
        .ok_or_else(|| {
            format!(
                "pending cancellation run {} disappeared from active observations",
                pending.run_id
            )
        })?;
    let cancellation = pending_run_cancellation(pending)?;
    let evidence = revalidate_capacity_preemption(
        actions,
        &observation,
        &cancellation,
        observed,
        &pending.front_head,
        &pending.opt_out_label,
    )?;
    let Some(evidence) = evidence else {
        return resolve_rejected_pending_intent(
            ledger,
            ledger_path,
            mutation_control,
            key,
            pending,
            was_uncertain,
        );
    };
    persist_capacity_evidence(
        &observation,
        &cancellation,
        &pending.front_head,
        &evidence,
        ledger_path,
        ledger,
    )?;
    if was_uncertain {
        supersede_pending_uncertainty(mutation_control, pending)?;
    }
    let correlation_id = MergeQueueMutationGuard::new_correlation_id();
    persist_pending_mutation_correlation(
        ledger,
        ledger_path,
        key,
        &correlation_id,
        PendingMutationKind::NormalCancel,
        "pending_normal_cancel_retry_intent",
    )?;
    let guard = acquire_pending_cancellation_guard_with_correlation(
        mutation_control,
        pending,
        &format!("runner steward retry cancel run {}", pending.run_id),
        &correlation_id,
    )?;
    read_current_pending_run_identity(actions, pending)?;
    actions
        .cancel_workflow_run(&pending.repo, pending.run_id)
        .map_err(|error| format!("exact normal cancellation retry failed: {error}"))?;
    let accepted = ledger
        .pending_cancellations
        .get_mut(key)
        .ok_or_else(|| "refreshed cancellation intent disappeared".to_owned())?;
    accepted.phase = PendingCancellationPhase::Accepted;
    record_audit(
        ledger,
        &pending.repo,
        &format!("capacity-run:{}", pending.run_id),
        "capacity_preemption_pending_after_recovery_acceptance",
    );
    save_ledger(ledger_path, ledger).map_err(|error| {
        format!(
            "normal cancellation retry accepted but pending phase persistence failed: {}",
            error.message
        )
    })?;
    guard
        .finish("cancel_accepted")
        .map_err(|error| format!("normal cancellation retry audit failed: {error}"))?;
    let accepted = ledger
        .pending_cancellations
        .get(key)
        .cloned()
        .ok_or_else(|| "accepted cancellation recovery record disappeared".to_owned())?;
    resume_pending_cancellation(
        actions,
        ledger_path,
        ledger,
        mutation_control,
        key,
        &accepted,
    )
}

fn resolve_rejected_pending_intent(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    mutation_control: &MutationControl,
    key: &str,
    pending: &PendingCancellation,
    was_uncertain: bool,
) -> Result<String, String> {
    if was_uncertain {
        return Err(format!(
            "cancellation intent for run {} no longer passes capacity-safety revalidation, \
             but mutation {} is uncertain; preserving pending state until terminal proof",
            pending.run_id, pending.mutation_correlation_id
        ));
    }
    mark_cancellation_skipped(ledger, ledger_path, key)?;
    supersede_pending_uncertainty(mutation_control, pending)?;
    clear_pending_cancellation(
        ledger,
        ledger_path,
        key,
        pending,
        "pending_intent_skipped_after_revalidation",
    )?;
    Ok("recovered_skipped_cancellation".to_owned())
}

fn pending_cancellation_reason(value: &str) -> Result<RunCancellationReason, String> {
    match value {
        "advisory_preamble_capacity_theft" => {
            Ok(RunCancellationReason::AdvisoryPreambleCapacityTheft)
        }
        "lower_priority_branch_preamble" => Ok(RunCancellationReason::LowerPriorityBranchPreamble),
        _ => Err(format!("unsupported pending cancellation reason `{value}`")),
    }
}

fn pending_run_cancellation(pending: &PendingCancellation) -> Result<RunCancellation, String> {
    Ok(RunCancellation {
        run_id: pending.run_id,
        reason: pending_cancellation_reason(&pending.reason)?,
    })
}

fn pending_uncertainty(
    control: &MutationControl,
    pending: &PendingCancellation,
) -> Result<bool, String> {
    let state_root = control
        .store
        .path()
        .parent()
        .unwrap_or(control.store.path());
    Ok(uncertain_mutations(state_root)?.iter().any(|entry| {
        entry.get("correlation_id").and_then(Value::as_str)
            == Some(pending.mutation_correlation_id.as_str())
    }))
}

fn supersede_pending_uncertainty(
    control: &MutationControl,
    pending: &PendingCancellation,
) -> Result<(), String> {
    if !pending_uncertainty(control, pending)? {
        return Ok(());
    }
    let state_root = control
        .store
        .path()
        .parent()
        .unwrap_or(control.store.path());
    supersede_uncertainty(
        state_root,
        &control.global_dir,
        &pending.mutation_correlation_id,
        &format!("steward durable {:?} recovery", pending.mutation_kind),
    )
}

fn read_pending_run(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<PendingRunState, String> {
    let run = read_pending_run_identity(actions, pending)?;
    let active_jobs = fetch_pending_run_jobs(actions, pending)?
        .into_iter()
        .filter(is_active_job)
        .collect::<Vec<_>>();
    let final_run = read_pending_run_identity(actions, pending)?;
    if final_run.status != run.status {
        return Err(format!(
            "pending cancellation run {} changed status during exact observation",
            pending.run_id
        ));
    }
    if run.status.eq_ignore_ascii_case("completed") && active_jobs.is_empty() {
        Ok(PendingRunState::Terminal)
    } else {
        Ok(PendingRunState::NonTerminal(NonTerminalRun {
            status: run.status,
            jobs: active_jobs,
        }))
    }
}

fn read_pending_run_identity(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<StewardRun, String> {
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!(
                "repos/{}/actions/runs/{}/attempts/{}",
                pending.repo, pending.run_id, pending.run_attempt
            ),
        ],
        "pending cancellation workflow run",
    )?;
    let run = parse_run(&value).ok_or_else(|| {
        format!(
            "pending cancellation run {} response is malformed",
            pending.run_id
        )
    })?;
    if run.id != pending.run_id
        || run.workflow_id != pending.workflow_id
        || run.run_attempt != pending.run_attempt
        || !run.head_sha.eq_ignore_ascii_case(&pending.head_sha)
        || run.head_branch != pending.head_branch
    {
        return Err(format!(
            "pending cancellation run {} immutable identity changed",
            pending.run_id
        ));
    }
    Ok(run)
}

fn read_current_pending_run_identity(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<(), String> {
    if !current_pending_run_identity_matches(actions, pending)? {
        return Err(format!(
            "current workflow run {} no longer matches pending attempt {}",
            pending.run_id, pending.run_attempt
        ));
    }
    Ok(())
}

fn current_pending_run_identity_matches(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<bool, String> {
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{}/actions/runs/{}", pending.repo, pending.run_id),
        ],
        "current pending cancellation workflow run",
    )?;
    let run = parse_run(&value).ok_or_else(|| {
        format!(
            "current pending cancellation run {} response is malformed",
            pending.run_id
        )
    })?;
    Ok(run.id == pending.run_id
        && run.workflow_id == pending.workflow_id
        && run.run_attempt == pending.run_attempt
        && run.head_sha.eq_ignore_ascii_case(&pending.head_sha)
        && run.head_branch == pending.head_branch)
}

fn fetch_pending_run_jobs(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<Vec<StewardJob>, String> {
    let mut all = Vec::new();
    for page in 1..=10 {
        let value = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{}/actions/runs/{}/attempts/{}/jobs?filter=all&per_page=100&page={page}",
                    pending.repo, pending.run_id, pending.run_attempt
                ),
            ],
            "pending cancellation workflow jobs",
        )?;
        let rows = value
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("workflow run {} response missing jobs", pending.run_id))?;
        let count = rows.len();
        for row in rows {
            all.push(parse_job(row)?);
        }
        if count < 100 {
            return Ok(all);
        }
    }
    Err(format!(
        "workflow run {} attempt {} exceeds 1000 jobs; refusing partial recovery scan",
        pending.run_id, pending.run_attempt
    ))
}

fn acquire_pending_cancellation_guard_with_correlation(
    control: &MutationControl,
    pending: &PendingCancellation,
    action: &str,
    correlation_id: &str,
) -> Result<MergeQueueMutationGuard, String> {
    let state = pending_cancellation_ship_state(pending);
    MergeQueueMutationGuard::acquire_in_mode_with_correlation(
        &control.store,
        &control.cwd,
        control.mode,
        &control.global_dir,
        &state,
        action,
        correlation_id,
    )
}

fn validate_pending_cancellation_authority(
    control: &MutationControl,
    pending: &PendingCancellation,
) -> Result<(), String> {
    MergeQueueMutationGuard::validate_in_mode(
        &control.store,
        &control.global_dir,
        &pending_cancellation_ship_state(pending),
    )
}

fn pending_cancellation_ship_state(pending: &PendingCancellation) -> ShipState {
    ShipState::new(
        pending.pr_number,
        &pending.repo,
        &pending.head_branch,
        &pending.base,
        &pending.head_sha,
        "runner-steward",
    )
}

fn clear_pending_cancellation(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    key: &str,
    pending: &PendingCancellation,
    action: &str,
) -> Result<(), String> {
    let Some(record) = ledger.pending_cancellations.remove(key) else {
        return Err(format!(
            "pending cancellation record for run {} disappeared",
            pending.run_id
        ));
    };
    record_audit(
        ledger,
        &pending.repo,
        &format!("capacity-run:{}", pending.run_id),
        action,
    );
    if let Err(error) = save_ledger(ledger_path, ledger) {
        ledger.pending_cancellations.insert(key.to_owned(), record);
        return Err(format!(
            "could not persist terminal pending-cancellation state: {}",
            error.message
        ));
    }
    Ok(())
}

fn complete_capacity_cancellation(
    context: &CapacityApplyContext<'_>,
    expected_front: &str,
    final_live: &StewardRun,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    record_audit(
        ledger,
        &context.observation.repo,
        &format!(
            "front:{expected_front}:capacity-run:{}:{}",
            context.cancellation.run_id, final_live.head_sha
        ),
        &format!(
            "capacity_preemption_accepted:{:?}",
            context.cancellation.reason
        ),
    );
    if let Err(error) = save_ledger(context.ledger_path, ledger) {
        return (
            Some("cancelled_after_job_revalidation".to_owned()),
            Some(format!(
                "cancel accepted but completion audit failed: {}",
                error.message
            )),
        );
    }
    match wait_for_run_terminalization(
        context.actions,
        &context.observation.repo,
        context.cancellation.run_id,
    ) {
        Ok(None) => {
            if let Err(error) = clear_pending_for_run(
                ledger,
                context.ledger_path,
                &context.observation.repo,
                final_live.id,
                "capacity_preemption_terminalized",
            ) {
                return (
                    Some("cancelled_terminal".to_owned()),
                    Some(format!(
                        "cancel terminalized but completion audit failed: {error}"
                    )),
                );
            }
            (Some("cancelled_terminal".to_owned()), None)
        }
        Ok(Some(active)) => {
            force_cancel_nonterminal_run(context, context.cancellation.run_id, &active, ledger)
        }
        Err(error) => {
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("capacity-run:{}", context.cancellation.run_id),
                "cancel_terminalization_unreadable",
            );
            let _ = save_ledger(context.ledger_path, ledger);
            (
                Some("cancel_terminalization_unreadable".to_owned()),
                Some(format!(
                    "cancel accepted but terminalization could not be verified: {error}"
                )),
            )
        }
    }
}

fn force_cancel_nonterminal_run(
    context: &CapacityApplyContext<'_>,
    run_id: u64,
    active: &NonTerminalRun,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let targets = active_runner_targets(&active.jobs);
    if let Err(error) = persist_force_cancel_intent(
        ledger,
        context.ledger_path,
        &context.observation.repo,
        run_id,
        &active.status,
        &targets,
    ) {
        return (
            Some("cancel_not_terminal".to_owned()),
            Some(format!(
                "cancel_not_terminal run {run_id} active={targets}; force-cancel intent persistence failed: {}",
                error.message
            )),
        );
    }
    let correlation_id = MergeQueueMutationGuard::new_correlation_id();
    if let Err(error) = persist_force_cancel_correlation(context, ledger, &correlation_id, run_id) {
        return (Some("cancel_not_terminal".to_owned()), Some(error));
    }
    let pending = ledger
        .pending_cancellations
        .values()
        .find(|pending| pending.repo == context.observation.repo && pending.run_id == run_id)
        .cloned()
        .expect("persist_force_cancel_correlation required pending record");
    let guard = match acquire_pending_cancellation_guard_with_correlation(
        context.mutation_control,
        &pending,
        &format!("runner steward force-cancel run {run_id}"),
        &correlation_id,
    ) {
        Ok(guard) => guard,
        Err(error) => {
            return (
                Some("cancel_not_terminal".to_owned()),
                Some(format!(
                    "cancel_not_terminal run {run_id} active={targets}; force-cancel authority failed: {error}"
                )),
            );
        }
    };
    if let Err(error) = revalidate_force_cancel_attempt(context, ledger, run_id) {
        return (
            Some("cancel_not_terminal".to_owned()),
            Some(format!(
                "exact force-cancel attempt revalidation failed: {error}"
            )),
        );
    }
    if let Err(error) = context
        .actions
        .force_cancel_workflow_run(&context.observation.repo, run_id)
    {
        audit_force_cancel_failure(
            ledger,
            context.ledger_path,
            &context.observation.repo,
            run_id,
        );
        return (
            Some("cancel_not_terminal".to_owned()),
            Some(format!(
                "cancel_not_terminal run {run_id} active={targets}; exact force-cancel failed: {error}"
            )),
        );
    }
    if let Err(error) = guard.finish("force_cancel_accepted") {
        return (
            Some("force_cancel_accepted_unverified".to_owned()),
            Some(format!(
                "force-cancel accepted for run {run_id}, but mutation audit failed: {error}"
            )),
        );
    }
    record_audit(
        ledger,
        &context.observation.repo,
        &format!("capacity-run:{run_id}"),
        "force_cancel_accepted",
    );
    if let Err(error) = save_ledger(context.ledger_path, ledger) {
        return (
            Some("force_cancel_accepted_unverified".to_owned()),
            Some(format!(
                "force-cancel accepted for run {run_id}, but audit persistence failed: {}",
                error.message
            )),
        );
    }
    verify_force_cancel_terminalization(context, run_id, ledger)
}

fn revalidate_force_cancel_attempt(
    context: &CapacityApplyContext<'_>,
    ledger: &StewardLedger,
    run_id: u64,
) -> Result<(), String> {
    let pending = ledger
        .pending_cancellations
        .values()
        .find(|pending| pending.repo == context.observation.repo && pending.run_id == run_id)
        .ok_or_else(|| "pending cancellation record disappeared before force-cancel".to_owned())?;
    read_current_pending_run_identity(context.actions, pending)
}

fn persist_force_cancel_correlation(
    context: &CapacityApplyContext<'_>,
    ledger: &mut StewardLedger,
    correlation_id: &str,
    run_id: u64,
) -> Result<(), String> {
    let key = ledger
        .pending_cancellations
        .iter()
        .find(|(_, pending)| pending.repo == context.observation.repo && pending.run_id == run_id)
        .map(|(key, _)| key.clone())
        .ok_or_else(|| {
            format!("cancel_not_terminal run {run_id}; pending cancellation record disappeared")
        })?;
    persist_pending_mutation_correlation(
        ledger,
        context.ledger_path,
        &key,
        correlation_id,
        PendingMutationKind::ForceCancel,
        "force_cancel_intent",
    )
}

fn verify_force_cancel_terminalization(
    context: &CapacityApplyContext<'_>,
    run_id: u64,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    match wait_for_run_terminalization(context.actions, &context.observation.repo, run_id) {
        Ok(None) => {
            match clear_pending_for_run(
                ledger,
                context.ledger_path,
                &context.observation.repo,
                run_id,
                "force_cancel_terminalized",
            ) {
                Ok(()) => (Some("force_cancelled_terminal".to_owned()), None),
                Err(error) => (
                    Some("force_cancelled_terminal".to_owned()),
                    Some(format!(
                        "force-cancel terminalized run {run_id}, but audit persistence failed: {error}"
                    )),
                ),
            }
        }
        Ok(Some(still_active)) => {
            let still_targets = active_runner_targets(&still_active.jobs);
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("capacity-run:{run_id}"),
                &format!("force_cancel_not_terminal:targets={still_targets}"),
            );
            let _ = save_ledger(context.ledger_path, ledger);
            (
                Some("force_cancel_not_terminal".to_owned()),
                Some(format!(
                    "force_cancel_not_terminal run {run_id} active={still_targets}; exact-host, exact-run recycle handoff required"
                )),
            )
        }
        Err(error) => {
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("capacity-run:{run_id}"),
                "force_cancel_terminalization_unreadable",
            );
            let audit_error = save_ledger(context.ledger_path, ledger).err();
            (
                Some("force_cancel_terminalization_unreadable".to_owned()),
                Some(format!(
                    "force-cancel accepted for run {run_id}, but terminalization is unreadable: {error}{}",
                    audit_error.map_or_else(String::new, |save_error| format!(
                        "; audit persistence also failed: {}",
                        save_error.message
                    ))
                )),
            )
        }
    }
}

fn clear_pending_for_run(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    repo: &str,
    run_id: u64,
    action: &str,
) -> Result<(), String> {
    let (key, pending) = ledger
        .pending_cancellations
        .iter()
        .find(|(_, pending)| pending.repo == repo && pending.run_id == run_id)
        .map(|(key, pending)| (key.clone(), pending.clone()))
        .ok_or_else(|| format!("pending cancellation record for run {run_id} disappeared"))?;
    clear_pending_cancellation(ledger, ledger_path, &key, &pending, action)
}

fn audit_force_cancel_failure(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    repo: &str,
    run_id: u64,
) {
    record_audit(
        ledger,
        repo,
        &format!("capacity-run:{run_id}"),
        "force_cancel_failed",
    );
    let _ = save_ledger(ledger_path, ledger);
}

fn persist_force_cancel_intent(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    repo: &str,
    run_id: u64,
    status: &str,
    targets: &str,
) -> Result<(), CliFailure> {
    record_audit(
        ledger,
        repo,
        &format!("capacity-run:{run_id}"),
        &format!("cancel_not_terminal:status={status}:targets={targets};force_cancel_intent"),
    );
    save_ledger(ledger_path, ledger)
}

fn wait_for_run_terminalization(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
) -> Result<Option<NonTerminalRun>, String> {
    let deadline = Instant::now() + CANCEL_TERMINAL_WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "cancel terminalization for run {run_id} reached its deadline"
            ));
        }
        let value = gh_json_timeout(
            actions,
            &[
                "api".to_owned(),
                format!("repos/{repo}/actions/runs/{run_id}"),
            ],
            "cancel terminalization",
            remaining,
        )?;
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| "cancel terminalization response missing status".to_owned())?
            .to_owned();
        let jobs = fetch_run_jobs_before(actions, repo, run_id, deadline)?;
        let active_jobs = jobs.into_iter().filter(is_active_job).collect::<Vec<_>>();
        if status == "completed" && active_jobs.is_empty() {
            return Ok(None);
        }
        if Instant::now() + CANCEL_TERMINAL_POLL >= deadline {
            return Ok(Some(NonTerminalRun {
                status,
                jobs: active_jobs,
            }));
        }
        thread::sleep(CANCEL_TERMINAL_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn is_active_job(job: &StewardJob) -> bool {
    matches!(
        job.status.as_str(),
        "queued" | "waiting" | "pending" | "requested" | "in_progress"
    )
}

fn active_runner_targets(jobs: &[StewardJob]) -> String {
    if jobs.is_empty() {
        return "workflow-status-only".to_owned();
    }
    jobs.iter()
        .map(|job| {
            format!(
                "{}@{}",
                job.name,
                job.runner_name.as_deref().unwrap_or("unassigned")
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn revalidate_capacity_preemption(
    actions: &GitHubActions,
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    observed: &StewardRun,
    expected_front: &str,
    opt_out_label: &str,
) -> Result<Option<CapacityRevalidation>, String> {
    let front_enqueued_at = match live_queue_front(actions, &observation.repo, &observation.base)? {
        Some((live_front, enqueued_at))
            if live_front.eq_ignore_ascii_case(expected_front)
                && timestamp_old_enough(&enqueued_at) =>
        {
            enqueued_at
        }
        _ => return Ok(None),
    };
    let Some(front_jobs) = live_queue_front_pool_jobs(actions, &observation.repo, expected_front)?
    else {
        return Ok(None);
    };
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!(
                "repos/{}/actions/runs/{}",
                observation.repo, cancellation.run_id
            ),
        ],
        "capacity-preemption run revalidation",
    )?;
    let mut live =
        parse_run(&value).ok_or_else(|| "live capacity-preemption run was malformed".to_owned())?;
    if !same_workflow_attempt(observed, &live) {
        return Ok(None);
    }
    live.jobs = fetch_run_jobs(actions, &observation.repo, cancellation.run_id)?;
    let candidate_pr_number = live
        .pull_request_number
        .ok_or_else(|| "capacity-preemption run no longer has a unique PR".to_owned())?;
    let Some(candidate_pr) = pull_request(
        actions,
        &observation.repo,
        candidate_pr_number,
        &BTreeMap::new(),
    )?
    else {
        return Ok(None);
    };
    let (mut all_current_heads, mut opted_out) = live_current_pull_request_state(
        actions,
        &observation.repo,
        &observation.base,
        opt_out_label,
    )?;
    all_current_heads.insert(candidate_pr.fact.number, candidate_pr.fact.head_sha.clone());
    if pull_request_opted_out(&candidate_pr, opt_out_label) {
        opted_out.insert(candidate_pr.fact.number);
    }
    let current_heads = if matches!(
        cancellation.reason,
        RunCancellationReason::LowerPriorityBranchPreamble
    ) {
        all_current_heads.clone()
    } else {
        BTreeMap::new()
    };
    if !is_safe_capacity_preemption(&live, &current_heads, &opted_out, cancellation.reason) {
        return Ok(None);
    }
    let current_pr_head = live
        .pull_request_number
        .and_then(|number| all_current_heads.get(&number).cloned());
    Ok(Some(CapacityRevalidation {
        candidate: live,
        front_enqueued_at,
        front_jobs,
        current_pr_head,
    }))
}

fn same_workflow_attempt(observed: &StewardRun, live: &StewardRun) -> bool {
    live.head_sha.eq_ignore_ascii_case(&observed.head_sha)
        && live.workflow_id == observed.workflow_id
        && live.run_attempt == observed.run_attempt
}

fn live_current_pull_request_state(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    opt_out_label: &str,
) -> Result<(BTreeMap<u64, String>, BTreeSet<u64>), String> {
    let prs = pull_requests(actions, repo, base, &BTreeMap::new())?;
    Ok((
        current_pull_request_heads(&prs),
        opted_out_pull_requests(&prs, opt_out_label),
    ))
}

fn pull_request(
    actions: &GitHubActions,
    repo: &str,
    number: u64,
    queue_positions: &BTreeMap<u64, u64>,
) -> Result<Option<ObservedPr>, String> {
    let value = gh_json(
        actions,
        &[
            "pr".to_owned(),
            "view".to_owned(),
            number.to_string(),
            "--repo".to_owned(),
            repo.to_owned(),
            "--json".to_owned(),
            "id,number,state,isDraft,headRefOid,headRefName,mergeStateStatus,autoMergeRequest,labels,statusCheckRollup".to_owned(),
        ],
        "capacity-preemption candidate PR",
    )?;
    if value.get("state").and_then(Value::as_str) != Some("OPEN") {
        return Ok(None);
    }
    parse_pr(&value, queue_positions).map(Some)
}

fn current_pull_request_heads(prs: &[ObservedPr]) -> BTreeMap<u64, String> {
    prs.iter()
        .map(|pr| (pr.fact.number, pr.fact.head_sha.clone()))
        .collect()
}

fn opted_out_pull_requests(prs: &[ObservedPr], opt_out_label: &str) -> BTreeSet<u64> {
    prs.iter()
        .filter(|pr| pull_request_opted_out(pr, opt_out_label))
        .map(|pr| pr.fact.number)
        .collect()
}

fn pull_request_opted_out(pr: &ObservedPr, opt_out_label: &str) -> bool {
    pr.fact
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(opt_out_label))
}

fn live_queue_front_pool_jobs(
    actions: &GitHubActions,
    repo: &str,
    expected_front: &str,
) -> Result<Option<Vec<StewardJob>>, String> {
    let mut runs = active_runs(actions, repo)?;
    for run in &mut runs {
        if run.event == "merge_group" && run.head_sha.eq_ignore_ascii_case(expected_front) {
            run.jobs = fetch_run_jobs(actions, repo, run.id)?;
        }
    }
    if !queue_front_waits_for_pool(&runs, expected_front) {
        return Ok(None);
    }
    Ok(Some(
        runs.into_iter()
            .filter(|run| {
                run.event == "merge_group" && run.head_sha.eq_ignore_ascii_case(expected_front)
            })
            .flat_map(|run| run.jobs)
            .collect(),
    ))
}

fn live_queue_front(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Option<(String, String)>, String> {
    let (enabled, positions, heads, enqueued) = merge_queue_snapshot(actions, repo, base)?;
    if !enabled {
        return Ok(None);
    }
    Ok(positions
        .iter()
        .min_by_key(|(_, position)| **position)
        .and_then(|(number, _)| Some((heads.get(number)?.clone(), enqueued.get(number)?.clone()))))
}

fn revalidate_coalescing_cancellation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    observed: &StewardRun,
    cancellation: &RunCancellation,
    opt_out_label: &str,
) -> Result<bool, String> {
    if !is_full_sha(&observed.head_sha) {
        return Ok(false);
    }
    let Some(pr_number) = observed
        .pull_request_number
        .or_else(|| merge_group_pr_number(observed))
    else {
        return Ok(false);
    };
    let Some(candidate_pr) = pull_request(actions, &observation.repo, pr_number, &BTreeMap::new())?
    else {
        return Ok(false);
    };
    if pull_request_opted_out(&candidate_pr, opt_out_label) {
        return Ok(false);
    }
    let mut current_heads = BTreeMap::new();
    current_heads.insert(pr_number, candidate_pr.fact.head_sha);
    let (_, _, merge_group_heads, _) =
        merge_queue_snapshot(actions, &observation.repo, &observation.base)?;
    let live_runs = active_runs(actions, &observation.repo)?;
    let opted_out = BTreeSet::new();
    let reason_reproved =
        plan_run_coalescing(&live_runs, &current_heads, &merge_group_heads, &opted_out)
            .iter()
            .any(|planned| {
                planned.run_id == cancellation.run_id && planned.reason == cancellation.reason
            });
    if !reason_reproved {
        return Ok(false);
    }
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!(
                "repos/{}/actions/runs/{}",
                observation.repo, cancellation.run_id
            ),
        ],
        "coalescing exact-run revalidation",
    )?;
    let Some(exact) = parse_run(&value) else {
        return Ok(false);
    };
    Ok(exact.status.eq_ignore_ascii_case("queued")
        && exact.workflow_id == observed.workflow_id
        && exact.event == observed.event
        && exact.head_sha.eq_ignore_ascii_case(&observed.head_sha)
        && exact
            .pull_request_number
            .or_else(|| merge_group_pr_number(&exact))
            == Some(pr_number))
}

fn merge_group_pr_number(run: &StewardRun) -> Option<u64> {
    if run.event != "merge_group" {
        return None;
    }
    run.head_branch
        .split_once("/pr-")
        .and_then(|(_, suffix)| suffix.split('-').next())
        .and_then(|number| number.parse().ok())
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

fn acquire_pr_mutation_guard(
    control: &MutationControl,
    observation: &RepoObservation,
    pr: &ObservedPr,
    action: &str,
) -> Result<MergeQueueMutationGuard, String> {
    let state = ShipState::new(
        pr.fact.number,
        &observation.repo,
        &pr.fact.head_branch,
        &observation.base,
        &pr.fact.head_sha,
        "runner-steward",
    );
    MergeQueueMutationGuard::acquire_in_mode(
        &control.store,
        &control.cwd,
        control.mode,
        &control.global_dir,
        &state,
        action,
    )
}

fn acquire_run_mutation_guard(
    control: &MutationControl,
    observation: &RepoObservation,
    run: &StewardRun,
    action: &str,
) -> Result<MergeQueueMutationGuard, String> {
    let pr_number = run
        .pull_request_number
        .or_else(|| merge_group_pr_number(run))
        .ok_or_else(|| {
            format!(
                "workflow run {} has no pull-request identity; refusing an unaudited mutation",
                run.id
            )
        })?;
    let branch = observation
        .prs
        .iter()
        .find(|pr| pr.fact.number == pr_number)
        .map_or(run.head_branch.as_str(), |pr| pr.fact.head_branch.as_str());
    let state = ShipState::new(
        pr_number,
        &observation.repo,
        branch,
        &observation.base,
        &run.head_sha,
        "runner-steward",
    );
    MergeQueueMutationGuard::acquire_in_mode(
        &control.store,
        &control.cwd,
        control.mode,
        &control.global_dir,
        &state,
        action,
    )
}

fn mutate_pr(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    if !matches!(
        decision,
        StewardDecision::ArmMergeQueue
            | StewardDecision::ExactHeadMerge
            | StewardDecision::RerunTransient { .. }
    ) {
        return (None, None);
    }
    let queue_positions = match merge_queue_snapshot(
        context.actions,
        &context.observation.repo,
        &context.observation.base,
    ) {
        Ok((_, positions, _, _)) => positions,
        Err(error) => return (None, Some(error)),
    };
    let live_pr = match pull_request(
        context.actions,
        &context.observation.repo,
        pr.fact.number,
        &queue_positions,
    ) {
        Ok(Some(live_pr)) => live_pr,
        Ok(None) => return (Some("skipped_after_live_revalidation".to_owned()), None),
        Err(error) => return (None, Some(error)),
    };
    let attempts = attempts_for(ledger, &context.observation.repo, &live_pr.fact);
    if classify_pr(&live_pr.fact, policy, &attempts) != *decision {
        return (Some("skipped_after_live_revalidation".to_owned()), None);
    }
    let pr = &live_pr;
    match decision {
        StewardDecision::ArmMergeQueue => enqueue_pull_request(context, pr, ledger),
        StewardDecision::ExactHeadMerge => exact_head_merge(context, pr, ledger),
        StewardDecision::RerunTransient { run_ids } => {
            mutate_transient_reruns(context, pr, policy, run_ids, ledger)
        }
        _ => (None, None),
    }
}

fn exact_head_merge(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let Some(merge_method) = context.observation.merge_method.as_deref() else {
        return (Some("waiting_merge_method_configuration".to_owned()), None);
    };
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        "runner steward exact-head merge",
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    let result = gh_json(
        context.actions,
        &[
            "api".to_owned(),
            "--method".to_owned(),
            "PUT".to_owned(),
            format!(
                "repos/{}/pulls/{}/merge",
                context.observation.repo, pr.fact.number
            ),
            "-f".to_owned(),
            format!("sha={}", pr.fact.head_sha),
            "-f".to_owned(),
            format!("merge_method={merge_method}"),
        ],
        "exact-head merge",
    );
    match result {
        Ok(value) if value.get("merged").and_then(Value::as_bool) == Some(true) => {
            if let Err(error) = guard.finish("merged") {
                return (
                    Some("merged".to_owned()),
                    Some(format!(
                        "merge succeeded but mutation audit failed: {error}"
                    )),
                );
            }
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha),
                "merge_exact_head",
            );
            (Some("merged".to_owned()), None)
        }
        Ok(value) => {
            let audit_error = guard
                .finish("rejected")
                .err()
                .map_or_else(String::new, |error| {
                    format!("; mutation audit also failed: {error}")
                });
            (
                None,
                Some(format!(
                    "GitHub refused exact-head merge: {}{audit_error}",
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown reason")
                )),
            )
        }
        Err(error) => (None, Some(error)),
    }
}

fn enqueue_pull_request(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        "runner steward enqueue pull request",
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    let query = "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head}){mergeQueueEntry{position}}}";
    let result = context.actions.run_gh(&[
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
            if let Err(error) = guard.finish("enqueued") {
                return (
                    Some("enqueued".to_owned()),
                    Some(format!(
                        "enqueue succeeded but mutation audit failed: {error}"
                    )),
                );
            }
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha),
                "enqueue_exact_head",
            );
            (Some("enqueued".to_owned()), None)
        }
        Err(error) => {
            let message = error.to_string();
            if enqueue_requirements_pending(&message) {
                match guard.finish("rejected_requirements") {
                    Ok(()) => (Some("waiting_enqueue_requirements".to_owned()), None),
                    Err(error) => (
                        Some("waiting_enqueue_requirements".to_owned()),
                        Some(format!(
                            "enqueue requirements rejected but mutation audit failed: {error}"
                        )),
                    ),
                }
            } else {
                (None, Some(message))
            }
        }
    }
}

fn mutate_transient_reruns(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    run_ids: &[u64],
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let mut errors = Vec::new();
    let mut rerun = Vec::new();
    for run_id in run_ids {
        let guard = match acquire_pr_mutation_guard(
            context.mutation_control,
            context.observation,
            pr,
            &format!("runner steward rerun failed run {run_id}"),
        ) {
            Ok(guard) => guard,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let key = attempt_key(
            &context.observation.repo,
            pr.fact.number,
            &pr.fact.head_sha,
            *run_id,
        );
        *ledger.transient_attempts.entry(key).or_default() += 1;
        record_audit(
            ledger,
            &context.observation.repo,
            &format!("run:{run_id}:{}", pr.fact.head_sha),
            "rerun_transient_intent",
        );
        if let Err(error) = save_ledger(context.ledger_path, ledger) {
            let audit_error = guard.finish("intent_persistence_failed").err();
            return (
                None,
                Some(format!(
                    "could not persist transient rerun intent: {}{}",
                    error.message,
                    audit_error.map_or_else(String::new, |error| format!(
                        "; mutation audit also failed: {error}"
                    ))
                )),
            );
        }
        match revalidate_transient_rerun(
            context.actions,
            context.observation,
            pr,
            policy,
            *run_id,
            ledger,
        ) {
            Ok(false) => {
                record_audit(
                    ledger,
                    &context.observation.repo,
                    &format!("run:{run_id}:{}", pr.fact.head_sha),
                    "rerun_transient_skipped_after_live_revalidation",
                );
                if let Err(error) = guard.finish("skipped_after_live_revalidation") {
                    errors.push(format!("rerun skip mutation audit failed: {error}"));
                }
                continue;
            }
            Err(error) => {
                let audit_error = guard.finish("revalidation_failed").err();
                errors.push(format!(
                    "{error}{}",
                    audit_error.map_or_else(String::new, |error| format!(
                        "; mutation audit also failed: {error}"
                    ))
                ));
                continue;
            }
            Ok(true) => {}
        }
        match context
            .actions
            .rerun_failed_run(&context.observation.repo, *run_id)
        {
            Ok(()) => {
                if let Err(error) = guard.finish("rerun_accepted") {
                    errors.push(format!(
                        "rerun accepted for run {run_id}, but mutation audit failed: {error}"
                    ));
                    continue;
                }
                rerun.push(*run_id);
                record_audit(
                    ledger,
                    &context.observation.repo,
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

fn revalidate_transient_rerun(
    actions: &GitHubActions,
    observation: &RepoObservation,
    observed_pr: &ObservedPr,
    policy: &StewardPolicy,
    run_id: u64,
    ledger: &StewardLedger,
) -> Result<bool, String> {
    let Some(live_pr) = pull_request(
        actions,
        &observation.repo,
        observed_pr.fact.number,
        &BTreeMap::new(),
    )?
    else {
        return Ok(false);
    };
    if !live_pr
        .fact
        .head_sha
        .eq_ignore_ascii_case(&observed_pr.fact.head_sha)
    {
        return Ok(false);
    }
    let mut attempts = attempts_for(ledger, &observation.repo, &live_pr.fact);
    if let Some(count) = attempts.get_mut(&run_id) {
        *count = count.saturating_sub(1);
    }
    let eligible = matches!(
        classify_pr(&live_pr.fact, policy, &attempts),
        StewardDecision::RerunTransient { run_ids } if run_ids.contains(&run_id)
    );
    if !eligible {
        return Ok(false);
    }
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{}/actions/runs/{run_id}", observation.repo),
        ],
        "transient exact-run revalidation",
    )?;
    let Some(run) = parse_run(&value) else {
        return Ok(false);
    };
    let conclusion = value
        .get("conclusion")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase();
    Ok(run.status.eq_ignore_ascii_case("completed")
        && run.head_sha.eq_ignore_ascii_case(&live_pr.fact.head_sha)
        && run.pull_request_number == Some(live_pr.fact.number)
        && matches!(
            conclusion.as_str(),
            "CANCELLED" | "TIMED_OUT" | "STARTUP_FAILURE" | "STALE"
        ))
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
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("could not open steward ledger {}: {error}", temp.display()),
            )
        })?;
    file.write_all(&payload).map_err(|error| {
        CliFailure::new(
            1,
            format!("could not write steward ledger {}: {error}", temp.display()),
        )
    })?;
    file.sync_all().map_err(|error| {
        CliFailure::new(
            1,
            format!("could not sync steward ledger {}: {error}", temp.display()),
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
    })?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                CliFailure::new(
                    1,
                    format!(
                        "could not sync steward state directory {}: {error}",
                        parent.display()
                    ),
                )
            })?;
    }
    Ok(())
}

fn render_report<W: Write>(
    stdout: &mut W,
    json_output: bool,
    apply: bool,
    ledger_path: &Path,
    reports: &[RepoReport],
) -> Result<(), CliFailure> {
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("apply".to_owned(), Value::from(apply));
        data.insert(
            "handoff_ledger".to_owned(),
            Value::from(ledger_path.display().to_string()),
        );
        data.insert(
            "repos".to_owned(),
            serde_json::to_value(reports).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        return write_json_envelope(stdout, "runner.steward", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "merge steward: mode={} handoff_ledger={}",
        if apply { "apply" } else { "dry-run" },
        ledger_path.display()
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

fn is_admin_protection_denied(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("http 403")
        && (lower.contains("must have admin rights")
            || lower.contains("administration permission")
            || lower.contains("resource not accessible by integration"))
}

fn enqueue_requirements_pending(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if [
        "http 401",
        "http 403",
        "http 429",
        "bad credentials",
        "resource not accessible by integration",
        "rate limit",
        "too many requests",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return false;
    }
    lower.contains("required status check")
        || lower.contains("required check")
        || lower.contains("required approving review")
        || lower.contains("required review")
        || lower.contains("requirements are not met")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_gh(temp: &tempfile::TempDir, body: &str) -> GitHubActions {
        use std::os::unix::fs::PermissionsExt;

        let path = temp.path().join("gh");
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write fake gh");
        let mut permissions = fs::metadata(&path).expect("fake gh metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod fake gh");
        GitHubActions::new(temp.path()).with_gh_binary_for_tests(path)
    }

    fn mutation_control(
        temp: &tempfile::TempDir,
        authority: &str,
        machine: &str,
    ) -> MutationControl {
        let global_dir = temp.path().join("global");
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&global_dir).expect("global config");
        fs::create_dir_all(&state_dir).expect("state");
        fs::write(
            global_dir.join("config.toml"),
            format!("[merge_queue]\nmutation_machine = \"{authority}\"\n"),
        )
        .expect("authority config");
        fs::write(state_dir.join("machine-tag"), format!("{machine}\n")).expect("machine tag");
        MutationControl {
            store: ShipStateStore::new(state_dir.join("ship")).expect("ship store"),
            cwd: temp.path().to_path_buf(),
            mode: RuntimeMode::Shipyard,
            global_dir,
        }
    }

    fn mutation_apply_context<'a>(
        actions: &'a GitHubActions,
        observation: &'a RepoObservation,
        ledger_path: &'a Path,
        mutation_control: &'a MutationControl,
    ) -> MutationApplyContext<'a> {
        MutationApplyContext {
            actions,
            observation,
            ledger_path,
            mutation_control,
        }
    }

    fn ready_pr() -> ObservedPr {
        parse_pr(
            &serde_json::json!({
                "id": "PR_kw",
                "number": 42,
                "state": "OPEN",
                "isDraft": false,
                "headRefOid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "headRefName": "feature",
                "mergeStateStatus": "CLEAN",
                "autoMergeRequest": null,
                "labels": [],
                "statusCheckRollup": [{
                    "__typename": "CheckRun",
                    "name": "macos",
                    "status": "COMPLETED",
                    "conclusion": "SUCCESS",
                    "detailsUrl": "https://github.com/owner/repo/actions/runs/100"
                }]
            }),
            &BTreeMap::new(),
        )
        .expect("ready PR")
    }

    fn observation_for(pr: ObservedPr, merge_queue: bool) -> RepoObservation {
        RepoObservation {
            repo: "owner/repo".to_owned(),
            base: "main".to_owned(),
            allow_auto_merge: merge_queue,
            merge_queue,
            merge_method: Some("merge".to_owned()),
            required_contexts: Vec::new(),
            prs: vec![pr],
            runs: Vec::new(),
            merge_group_heads: BTreeMap::new(),
            merge_group_enqueued_at: BTreeMap::new(),
            preemption_error: None,
        }
    }

    fn queue_policy() -> StewardPolicy {
        StewardPolicy {
            merge_queue: true,
            native_auto_merge: true,
            required_contexts: Vec::new(),
            opt_out_label: "steward:skip".to_owned(),
            max_transient_reruns: 1,
        }
    }

    fn queued_run(id: u64, created_at: &str) -> StewardRun {
        StewardRun {
            id,
            workflow_id: 77,
            run_attempt: 1,
            workflow: "Required".to_owned(),
            head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            head_branch: "feature".to_owned(),
            status: "queued".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(42),
            created_at: created_at.to_owned(),
            jobs: Vec::new(),
        }
    }

    fn pending_cancellation_record() -> PendingCancellation {
        PendingCancellation {
            repo: "owner/repo".to_owned(),
            base: "main".to_owned(),
            run_id: 100,
            workflow_id: 77,
            run_attempt: 1,
            head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            head_branch: "feature".to_owned(),
            pr_number: 42,
            front_head: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            initiated_at: "2026-07-26T00:00:00Z".to_owned(),
            phase: PendingCancellationPhase::Accepted,
            mutation_correlation_id: "finished-test-mutation".to_owned(),
            mutation_kind: PendingMutationKind::NormalCancel,
            reason: "advisory_preamble_capacity_theft".to_owned(),
            opt_out_label: "steward:skip".to_owned(),
        }
    }

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
    fn repository_settings_supply_canonical_guard_identity() {
        assert_eq!(
            canonical_repo_name(&serde_json::json!({"full_name": "Owner/Repo"}))
                .expect("canonical"),
            "Owner/Repo"
        );
        assert!(canonical_repo_name(&serde_json::json!({})).is_err());
        assert!(
            canonical_repo_name(&serde_json::json!({"full_name": "owner/repo/extra"})).is_err()
        );
    }

    #[test]
    fn entitlement_match_is_exact_enough_not_to_swallow_generic_forbidden() {
        assert!(is_private_free_entitlement(
            "Upgrade to GitHub Pro or make this repository public to enable this feature."
        ));
        assert!(!is_private_free_entitlement("HTTP 403 forbidden"));
        assert!(is_admin_protection_denied(
            "HTTP 403: Must have admin rights to Repository"
        ));
        assert!(!is_admin_protection_denied("HTTP 403 forbidden"));
    }

    #[test]
    fn job_parser_and_reason_labels_fail_closed_and_stay_stable() {
        let parsed = parse_job(&serde_json::json!({
            "name": "macos",
            "status": "in_progress",
            "labels": ["self-hosted", "pulp-preamble"],
            "runner_name": "pulp-preamble-m5"
        }))
        .expect("job");
        assert_eq!(parsed.labels[1], "pulp-preamble");
        assert!(parse_job(&serde_json::json!({"status": "queued"})).is_err());
        assert_eq!(
            cancellation_reason_label(RunCancellationReason::LowerPriorityBranchPreamble),
            "lower_priority_branch_preamble"
        );
    }

    #[test]
    fn evaluated_rules_extract_required_contexts_and_reject_malformed_payloads() {
        let contexts = evaluated_required_contexts(&serde_json::json!([[
                {
                    "type": "required_status_checks",
                    "parameters": {
                        "required_status_checks": [
                            {"context": "macos"},
                            {"context": "linux"},
                            {"context": "macos"}
                        ]
                    }
                }
            ],
            [
                {"type": "pull_request", "parameters": {"required_approving_review_count": 1}}
            ]
        ]))
        .expect("rules");
        assert_eq!(contexts, vec!["linux", "macos"]);
        assert!(evaluated_required_contexts(&serde_json::json!({})).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn required_context_transport_unions_classic_checks_and_paginated_rules() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            r#"
case "$*" in
  *"protection/required_status_checks"*)
    printf '%s' '{"contexts":["classic"],"checks":[{"context":"app-bound"}]}' ;;
  *"rules/branches/main --paginate --slurp"*)
    printf '%s' '[[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"rules-a"}]}}],[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"rules-b"}]}}]]' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        );

        assert_eq!(
            required_contexts(&actions, "owner/repo", "main").expect("required contexts"),
            vec!["app-bound", "classic", "rules-a", "rules-b"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn pull_request_transport_preserves_fresh_queue_position() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            r#"printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[]}'"#,
        );
        let positions = BTreeMap::from([(42, 3)]);

        let pr = pull_request(&actions, "owner/repo", 42, &positions)
            .expect("transport")
            .expect("open PR");
        assert_eq!(pr.fact.queue_position, Some(3));
    }

    #[cfg(unix)]
    #[test]
    fn merge_queue_transport_refuses_partial_snapshot() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            r#"printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":true}}}}}}'"#,
        );

        let error = merge_queue_snapshot(&actions, "owner/repo", "main").expect_err("partial");
        assert!(error.contains("exceeds 100 entries"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn active_run_transport_deduplicates_status_and_page_overlap() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            r#"
case "$*" in
  *"actions/runs?status=queued"*|*"actions/runs?status=waiting"*)
    printf '%s' '{"workflow_runs":[{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":42}]}]}' ;;
  *"actions/runs?status="*) printf '%s' '{"workflow_runs":[]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        );

        let runs = active_runs(&actions, "owner/repo").expect("active runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, 1);
    }

    #[test]
    fn overlapping_apply_pass_fails_fast_on_ledger_lock() {
        let temp = tempfile::tempdir().expect("temp");
        let ledger = temp.path().join("merge-steward.json");
        let _first = acquire_ledger_lock(&ledger).expect("first lock");
        let error = acquire_ledger_lock(&ledger).expect_err("second lock must not block");
        assert!(
            error.message.contains("already running"),
            "{}",
            error.message
        );
    }

    #[test]
    fn final_ledger_failure_is_renderable_and_marks_tick_unhealthy() {
        let temp = tempfile::tempdir().expect("temp");
        let parent_file = temp.path().join("not-a-directory");
        fs::write(&parent_file, "occupied").expect("parent file");
        let ledger_path = parent_file.join("merge-steward.json");
        let mut reports = Vec::new();
        let mut unhealthy = false;

        persist_final_ledger(
            &ledger_path,
            &StewardLedger::default(),
            "main",
            &mut reports,
            &mut unhealthy,
        );

        assert!(unhealthy);
        assert_eq!(reports.len(), 1);
        assert!(reports[0].errors[0].contains("ledger persistence failed"));
        let mut output = Vec::new();
        render_report(&mut output, true, true, &ledger_path, &reports).expect("render");
        assert!(
            String::from_utf8(output)
                .expect("UTF-8")
                .contains("ledger persistence failed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn enqueue_transport_mutates_only_after_live_queue_and_head_revalidation() {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{{"data":{{"enqueuePullRequest":{{"mergeQueueEntry":{{"position":1}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
                log.display()
            ),
        );
        let pr = ready_pr();
        let observation = observation_for(pr.clone(), true);
        let policy = queue_policy();
        let mut ledger = StewardLedger::default();
        let mutation_control = mutation_control(&temp, "studio", "studio");
        let ledger_path = temp.path().join("ledger.json");
        let context =
            mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

        let (mutation, error) = mutate_pr(
            &context,
            &pr,
            &policy,
            &StewardDecision::ArmMergeQueue,
            &mut ledger,
        );

        assert_eq!(mutation.as_deref(), Some("enqueued"));
        assert!(error.is_none(), "{error:?}");
        let calls = fs::read_to_string(log).expect("calls");
        assert!(calls.contains("mergeQueue"), "{calls}");
        assert!(calls.contains("pr view 42"), "{calls}");
        assert!(calls.contains("enqueuePullRequest"), "{calls}");
    }

    #[cfg(unix)]
    #[test]
    fn steward_apply_rejects_unauthorized_host_before_remote_mutation() {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"enqueuePullRequest"*) echo "mutation must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
                log.display()
            ),
        );
        let pr = ready_pr();
        let observation = observation_for(pr.clone(), true);
        let mut ledger = StewardLedger::default();
        let mutation_control = mutation_control(&temp, "studio", "m1");
        let ledger_path = temp.path().join("ledger.json");
        let context =
            mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

        let (mutation, error) = mutate_pr(
            &context,
            &pr,
            &queue_policy(),
            &StewardDecision::ArmMergeQueue,
            &mut ledger,
        );

        assert!(mutation.is_none());
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.contains("authority is `studio`")),
            "{error:?}"
        );
        let calls = fs::read_to_string(log).expect("calls");
        assert!(!calls.contains("enqueuePullRequest"), "{calls}");
    }

    #[cfg(unix)]
    #[test]
    fn unauthorized_steward_does_not_consume_transient_rerun_budget() {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"TIMED_OUT","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"rerun-failed-jobs"*) echo "mutation must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
                log.display()
            ),
        );
        let mut pr = ready_pr();
        pr.fact.checks[0].conclusion = Some("TIMED_OUT".to_owned());
        let observation = observation_for(pr.clone(), true);
        let mut ledger = StewardLedger::default();
        let mutation_control = mutation_control(&temp, "studio", "m1");
        let ledger_path = temp.path().join("ledger.json");
        let context =
            mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

        let (mutation, error) = mutate_pr(
            &context,
            &pr,
            &queue_policy(),
            &StewardDecision::RerunTransient { run_ids: vec![100] },
            &mut ledger,
        );

        assert!(mutation.is_none());
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.contains("authority is `studio`")),
            "{error:?}"
        );
        assert!(ledger.transient_attempts.is_empty());
        let calls = fs::read_to_string(log).expect("calls");
        assert!(!calls.contains("rerun-failed-jobs"), "{calls}");
    }

    #[test]
    fn unauthorized_steward_does_not_consume_capacity_preemption_budget() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = GitHubActions::new(temp.path());
        let mut pr = ready_pr();
        pr.fact.queue_position = Some(1);
        let mut observation = observation_for(pr, true);
        observation.merge_group_heads.insert(42, "b".repeat(40));
        observation.merge_group_enqueued_at.insert(
            42,
            (Utc::now() - chrono::Duration::minutes(20)).to_rfc3339(),
        );
        observation.runs = vec![queued_run(100, "2026-07-26T00:00:00Z")];
        let cancellation = RunCancellation {
            run_id: 100,
            reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
        };
        let mut ledger = StewardLedger::default();
        let mutation_control = mutation_control(&temp, "studio", "m1");
        let ledger_path = temp.path().join("ledger.json");
        let context = CapacityApplyContext {
            actions: &actions,
            observation: &observation,
            cancellation: &cancellation,
            ledger_path: &ledger_path,
            mutation_control: &mutation_control,
        };

        let (mutation, error) = apply_capacity_preemption(&context, "steward:skip", &mut ledger);

        assert!(mutation.is_none());
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.contains("authority is `studio`")),
            "{error:?}"
        );
        assert!(ledger.preemption_attempts.is_empty());
        assert!(ledger.audit.is_empty());
    }

    #[test]
    fn capacity_revalidation_rejects_a_new_workflow_attempt() {
        let observed = queued_run(100, "2026-07-26T00:00:00Z");
        let mut rerun = observed.clone();
        rerun.run_attempt += 1;

        assert!(!same_workflow_attempt(&observed, &rerun));
    }

    #[test]
    fn initial_capacity_guard_correlation_is_durable_before_started_audit_can_be_orphaned() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = GitHubActions::new(temp.path());
        let mut observation = observation_for(ready_pr(), true);
        let run = queued_run(100, "2026-07-26T00:00:00Z");
        observation.runs = vec![run.clone()];
        let cancellation = RunCancellation {
            run_id: 100,
            reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
        };
        let ledger_path = temp.path().join("ledger.json");
        let control = mutation_control(&temp, "studio", "studio");
        let context = CapacityApplyContext {
            actions: &actions,
            observation: &observation,
            cancellation: &cancellation,
            ledger_path: &ledger_path,
            mutation_control: &control,
        };
        let mut ledger = StewardLedger::default();

        let (guard, pending) = start_capacity_preemption(
            &context,
            "steward:skip",
            &mut ledger,
            &run,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect("durable guard start");
        let persisted = load_ledger(&ledger_path).expect("persisted ledger");
        assert_eq!(
            persisted
                .pending_cancellations
                .get(&pending_cancellation_key(&pending))
                .expect("pending before crash")
                .mutation_correlation_id,
            guard.correlation_id()
        );

        drop(guard);

        let uncertain = uncertain_mutations(control.store.path().parent().expect("state root"))
            .expect("uncertainty");
        assert_eq!(
            uncertain[0]["correlation_id"].as_str(),
            Some(pending.mutation_correlation_id.as_str())
        );
    }

    #[test]
    fn skipped_capacity_intent_recovers_without_sending_a_cancellation() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = GitHubActions::new(temp.path());
        let mut observation = observation_for(ready_pr(), true);
        let run = queued_run(100, "2026-07-26T00:00:00Z");
        observation.runs = vec![run.clone()];
        let cancellation = RunCancellation {
            run_id: 100,
            reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
        };
        let ledger_path = temp.path().join("ledger.json");
        let control = mutation_control(&temp, "studio", "studio");
        let context = CapacityApplyContext {
            actions: &actions,
            observation: &observation,
            cancellation: &cancellation,
            ledger_path: &ledger_path,
            mutation_control: &control,
        };
        let mut ledger = StewardLedger::default();
        let (guard, pending) = start_capacity_preemption(
            &context,
            "steward:skip",
            &mut ledger,
            &run,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect("durable guard start");
        let key = pending_cancellation_key(&pending);
        mark_cancellation_skipped(&mut ledger, &ledger_path, &key).expect("durable tombstone");

        drop(guard);

        let mut recovered = load_ledger(&ledger_path).expect("persisted tombstone");
        let tombstone = recovered
            .pending_cancellations
            .get(&key)
            .expect("skipped pending")
            .clone();
        assert_eq!(tombstone.phase, PendingCancellationPhase::Skipped);
        assert_eq!(
            resume_pending_cancellation(
                &actions,
                &ledger_path,
                &mut recovered,
                &control,
                &key,
                &tombstone,
            )
            .expect("skip recovery"),
            "recovered_skipped_cancellation"
        );
        assert!(recovered.pending_cancellations.is_empty());
        assert!(
            uncertain_mutations(control.store.path().parent().expect("state root"))
                .expect("uncertainty")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn post_accepted_crash_does_not_clear_uncertain_intent_when_evidence_disappears() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            r#"
case "$*" in
  *"actions/runs/100/cancel") exit 0 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        );
        let control = mutation_control(&temp, "studio", "studio");
        let mut pending = pending_cancellation_record();
        pending.phase = PendingCancellationPhase::Intent;
        let correlation_id = MergeQueueMutationGuard::new_correlation_id();
        let guard = acquire_pending_cancellation_guard_with_correlation(
            &control,
            &pending,
            "runner steward preempt capacity run 100",
            &correlation_id,
        )
        .expect("guard");
        correlation_id.clone_into(&mut pending.mutation_correlation_id);
        let key = pending_cancellation_key(&pending);
        let ledger_path = temp.path().join("ledger.json");
        let mut ledger = StewardLedger {
            pending_cancellations: BTreeMap::from([(key.clone(), pending.clone())]),
            ..StewardLedger::default()
        };
        save_ledger(&ledger_path, &ledger).expect("intent persisted");

        actions
            .cancel_workflow_run(&pending.repo, pending.run_id)
            .expect("POST accepted");
        drop(guard);

        assert!(pending_uncertainty(&control, &pending).expect("uncertain"));
        let error = resolve_rejected_pending_intent(
            &mut ledger,
            &ledger_path,
            &control,
            &key,
            &pending,
            true,
        )
        .expect_err("uncertain POST cannot become skipped");
        assert!(error.contains("preserving pending state"), "{error}");
        assert_eq!(
            ledger
                .pending_cancellations
                .get(&key)
                .expect("pending preserved")
                .phase,
            PendingCancellationPhase::Intent
        );
        assert!(
            load_ledger(&ledger_path)
                .expect("reload")
                .pending_cancellations
                .contains_key(&key)
        );
        assert!(pending_uncertainty(&control, &pending).expect("uncertainty preserved"));
    }

    #[cfg(unix)]
    #[test]
    fn pending_capacity_cancellation_resumes_after_cancel_accepted_restart() {
        let temp = tempfile::tempdir().expect("temp");
        let calls = temp.path().join("calls");
        let reads = temp.path().join("reads");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"/force-cancel") exit 0 ;;
  *"/jobs?"*) printf '%s' '{{"jobs":[]}}' ;;
  *"actions/runs/100/attempts/1"|*"actions/runs/100")
    count=0
    test ! -f '{}' || count=$(cat '{}')
    count=$((count + 1))
    printf '%s' "$count" > '{}'
    if test "$count" -le 5; then status=in_progress; else status=completed; fi
    printf '%s' '{{"id":100,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"'"$status"'","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
                calls.display(),
                reads.display(),
                reads.display(),
                reads.display(),
            ),
        );
        let control = mutation_control(&temp, "studio", "studio");
        let mut pending = pending_cancellation_record();
        let correlation_id = MergeQueueMutationGuard::new_correlation_id();
        let interrupted_guard = acquire_pending_cancellation_guard_with_correlation(
            &control,
            &pending,
            "runner steward preempt capacity run 100",
            &correlation_id,
        )
        .expect("interrupted guard");
        interrupted_guard
            .correlation_id()
            .clone_into(&mut pending.mutation_correlation_id);
        drop(interrupted_guard);
        let key = pending_cancellation_key(&pending);
        let mut ledger = StewardLedger {
            preemption_attempts: BTreeMap::from([(
                "owner/repo:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                1,
            )]),
            pending_cancellations: BTreeMap::from([(key, pending)]),
            ..StewardLedger::default()
        };
        let ledger_path = temp.path().join("ledger.json");
        save_ledger(&ledger_path, &ledger).expect("seed ledger");

        let (errors, cancellations) =
            resume_pending_cancellations(&actions, &ledger_path, &mut ledger, &control);

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(cancellations["owner/repo"][0].run_id, 100);
        assert!(ledger.pending_cancellations.is_empty());
        assert_eq!(ledger.preemption_attempts.values().copied().sum::<u32>(), 1);
        let calls = fs::read_to_string(calls).expect("calls");
        assert!(calls.contains("/force-cancel"), "{calls}");
        let reloaded = load_ledger(&ledger_path).expect("reload");
        assert!(reloaded.pending_cancellations.is_empty());
        assert!(
            uncertain_mutations(control.store.path().parent().expect("state root"))
                .expect("uncertainties")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn pending_cancellation_survives_transient_read_failure_then_recovers() {
        let temp = tempfile::tempdir().expect("temp");
        let ledger_path = temp.path().join("ledger.json");
        let pending = pending_cancellation_record();
        let key = pending_cancellation_key(&pending);
        let mut ledger = StewardLedger {
            pending_cancellations: BTreeMap::from([(key.clone(), pending)]),
            ..StewardLedger::default()
        };
        save_ledger(&ledger_path, &ledger).expect("seed ledger");
        let control = mutation_control(&temp, "studio", "studio");
        let failing = fake_gh(&temp, r#"echo "temporary read failure" >&2; exit 1"#);

        let (errors, cancellations) =
            resume_pending_cancellations(&failing, &ledger_path, &mut ledger, &control);

        assert_eq!(errors.len(), 1);
        assert!(cancellations.is_empty());
        assert!(ledger.pending_cancellations.contains_key(&key));
        assert!(
            load_ledger(&ledger_path)
                .expect("reload failed recovery")
                .pending_cancellations
                .contains_key(&key)
        );

        let reads = temp.path().join("reads");
        let recovered = fake_gh(
            &temp,
            &format!(
                r#"
case "$*" in
  *"/force-cancel") exit 0 ;;
  *"/jobs?"*) printf '%s' '{{"jobs":[]}}' ;;
  *"actions/runs/100/attempts/1"|*"actions/runs/100")
    count=0
    test ! -f '{}' || count=$(cat '{}')
    count=$((count + 1))
    printf '%s' "$count" > '{}'
    if test "$count" -le 5; then status=in_progress; else status=completed; fi
    printf '%s' '{{"id":100,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"'"$status"'","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
                reads.display(),
                reads.display(),
                reads.display(),
            ),
        );

        let (errors, cancellations) =
            resume_pending_cancellations(&recovered, &ledger_path, &mut ledger, &control);

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(cancellations["owner/repo"][0].run_id, 100);
        assert!(ledger.pending_cancellations.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn steward_apply_obeys_central_hold_before_remote_mutation() {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"enqueuePullRequest"*) echo "mutation must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
                log.display()
            ),
        );
        let pr = ready_pr();
        let observation = observation_for(pr.clone(), true);
        let mut ledger = StewardLedger::default();
        let mutation_control = mutation_control(&temp, "studio", "studio");
        let state_root = mutation_control.store.path().parent().expect("state root");
        crate::merge_queue_control::hold(state_root, "incident").expect("hold");
        let ledger_path = temp.path().join("ledger.json");
        let context =
            mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

        let (mutation, error) = mutate_pr(
            &context,
            &pr,
            &queue_policy(),
            &StewardDecision::ArmMergeQueue,
            &mut ledger,
        );

        assert!(mutation.is_none());
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.contains("centrally held")),
            "{error:?}"
        );
        let calls = fs::read_to_string(log).expect("calls");
        assert!(!calls.contains("enqueuePullRequest"), "{calls}");
    }

    #[cfg(unix)]
    #[test]
    fn steward_ambiguous_failure_is_durable_shared_uncertainty() {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"enqueuePullRequest"*) echo "connection reset after request body" >&2; exit 1 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
                log.display()
            ),
        );
        let pr = ready_pr();
        let observation = observation_for(pr.clone(), true);
        let mut ledger = StewardLedger::default();
        let mutation_control = mutation_control(&temp, "studio", "studio");
        let ledger_path = temp.path().join("ledger.json");
        let context =
            mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

        let first = mutate_pr(
            &context,
            &pr,
            &queue_policy(),
            &StewardDecision::ArmMergeQueue,
            &mut ledger,
        );
        assert!(first.0.is_none());
        assert!(first.1.is_some());
        let state_root = mutation_control.store.path().parent().expect("state root");
        let uncertain =
            crate::merge_queue_control::uncertain_mutations(state_root).expect("uncertainty");
        assert_eq!(uncertain.len(), 1);
        assert_eq!(
            uncertain[0]["action"],
            "runner steward enqueue pull request"
        );
        assert_eq!(uncertain[0]["pr"], 42);

        let second = mutate_pr(
            &context,
            &pr,
            &queue_policy(),
            &StewardDecision::ArmMergeQueue,
            &mut ledger,
        );
        assert!(
            second
                .1
                .as_deref()
                .is_some_and(|message| message.contains("is uncertain")),
            "{second:?}"
        );
        let calls = fs::read_to_string(log).expect("calls");
        assert_eq!(calls.matches("enqueuePullRequest").count(), 1, "{calls}");
    }

    #[cfg(unix)]
    #[test]
    fn steward_dry_run_needs_no_mutation_authority_and_makes_no_remote_write() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(&temp, r#"echo "unexpected mutation: $*" >&2; exit 90"#);
        let pr = ready_pr();
        let observation = observation_for(pr, true);
        let args = StewardCommandArgs {
            repos: vec!["owner/repo".to_owned()],
            base: "main".to_owned(),
            opt_out_label: "steward:skip".to_owned(),
            max_transient_reruns: 1,
            coalesce: true,
            preempt_capacity: true,
            max_preemptions_per_head: 1,
            apply: false,
            ledger: None,
        };
        let mut ledger = StewardLedger::default();

        let (reports, failed) = apply_pr_plans(
            &actions,
            &args,
            &observation,
            &queue_policy(),
            &temp.path().join("ledger.json"),
            &mut ledger,
            None,
        );

        assert!(!failed);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].decision, StewardDecision::ArmMergeQueue);
        assert!(reports[0].mutation.is_none());
        assert!(reports[0].error.is_none());
        assert!(!temp.path().join("state/ship").exists());
    }

    #[cfg(unix)]
    #[test]
    fn enqueue_requirements_refusal_is_waiting_not_control_plane_failure() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            r#"
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  "pr view "*)
    printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"BLOCKED","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}]}' ;;
  *"enqueuePullRequest"*)
    echo "Required approving review has not been submitted" >&2
    exit 1 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        );
        let mut pr = ready_pr();
        pr.fact.merge_state = "BLOCKED".to_owned();
        let observation = observation_for(pr.clone(), true);
        let policy = queue_policy();
        let mut ledger = StewardLedger::default();
        let mutation_control = mutation_control(&temp, "studio", "studio");
        let ledger_path = temp.path().join("ledger.json");
        let context =
            mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

        let (mutation, error) = mutate_pr(
            &context,
            &pr,
            &policy,
            &StewardDecision::ArmMergeQueue,
            &mut ledger,
        );

        assert_eq!(mutation.as_deref(), Some("waiting_enqueue_requirements"));
        assert!(error.is_none(), "{error:?}");
        assert!(!enqueue_requirements_pending(
            "HTTP 403: Resource not accessible by integration: required review"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn queued_duplicate_cancel_transport_reproves_exact_run_before_mutation() {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":null}}}}}}' ;;
  *"actions/runs?status=queued"*)
    printf '%s' '{{"workflow_runs":[
      {{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}},
      {{"id":2,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T01:00:00Z","pull_requests":[{{"number":42}}]}}
    ]}}' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  "api repos/owner/repo/actions/runs/1")
    printf '%s' '{{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
  *"actions/runs/1/cancel"*) printf '%s' '{{}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
                log.display()
            ),
        );
        let pr = ready_pr();
        let mut observation = observation_for(pr, true);
        observation.runs = vec![
            queued_run(1, "2026-07-26T00:00:00Z"),
            queued_run(2, "2026-07-26T01:00:00Z"),
        ];
        let cancellation = RunCancellation {
            run_id: 1,
            reason: RunCancellationReason::DuplicateImmutableHead,
        };
        let mut ledger = StewardLedger::default();
        let mutation_control = mutation_control(&temp, "studio", "studio");

        let (mutation, error) = apply_run_cancellation(
            &actions,
            &observation,
            &cancellation,
            "steward:skip",
            &temp.path().join("ledger.json"),
            &mut ledger,
            &mutation_control,
        );

        assert_eq!(mutation.as_deref(), Some("cancelled"));
        assert!(error.is_none(), "{error:?}");
        let calls = fs::read_to_string(log).expect("calls");
        let exact = calls
            .find("api repos/owner/repo/actions/runs/1\n")
            .expect("exact run re-read");
        let cancel = calls
            .find("api -X POST repos/owner/repo/actions/runs/1/cancel")
            .expect("cancel call");
        assert!(exact < cancel, "{calls}");
    }
}
