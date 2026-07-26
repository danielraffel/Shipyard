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
use crate::merge_queue_control::{DurableMutationIntent, MergeQueueMutationGuard};
use crate::merge_steward::{
    CapacityPreemptionPolicy, QueueFrontPressure, RunCancellation, RunCancellationReason,
    StewardCheck, StewardDecision, StewardJob, StewardPolicy, StewardPullRequest, StewardRun,
    classify_pr, is_capacity_preemption_workflow, is_full_sha, is_safe_capacity_preemption,
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
    capacity_preemption_policy: CapacityPreemptionPolicy,
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

mod cancellation;
mod cancellation_recovery;
mod cancellation_revalidation;
mod cancellation_terminalization;
mod capacity_cancellation;
mod ledger;
mod observation;
mod pr_mutations;
mod render;

use cancellation::{
    apply_repo_plan, cancellation_reason_label, queue_front_head, timestamp_old_enough,
};
use cancellation_recovery::resume_pending_cancellations;
use cancellation_revalidation::{
    acquire_pr_mutation_guard, acquire_run_mutation_guard, attempts_for,
    current_pull_request_heads, merge_group_pr_number, opted_out_pull_requests, pull_request,
    revalidate_capacity_preemption, revalidate_coalescing_cancellation,
};
use cancellation_terminalization::{
    acquire_pending_cancellation_guard, active_runner_targets, clear_pending_cancellation,
    complete_capacity_cancellation, current_pending_run_identity_matches,
    persist_force_cancel_intent, read_current_pending_run_identity, read_pending_run,
    validate_pending_cancellation_authority,
};
use capacity_cancellation::{
    apply_capacity_preemption, mark_cancellation_skipped, persist_capacity_evidence,
};
use ledger::{attempt_key, load_ledger, record_audit, save_ledger};
use observation::{
    active_runs, fetch_run_jobs, fetch_run_jobs_before, gh_json, gh_json_timeout,
    merge_queue_snapshot, observe_repo, parse_job, parse_pr, parse_run, pull_requests,
    resolve_repos,
};
use pr_mutations::mutate_pr;
use render::{
    enqueue_requirements_pending, is_admin_protection_denied, is_private_free_entitlement,
    render_report,
};

#[cfg(test)]
mod tests;
