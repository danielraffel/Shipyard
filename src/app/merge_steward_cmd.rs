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
use serde_json::{Value, json};

use super::CliFailure;
use crate::cloud::GitHubActions;
use crate::identity::RuntimeMode;
use crate::merge_queue_control::{
    DurableMutationIntent, MergeQueueMutationGuard, authority_status, lock_is_contended,
};
use crate::merge_steward::{
    CapacityPreemptionPolicy, QueueFrontPressure, RequiredCheck, RunCancellation,
    RunCancellationReason, StewardCheck, StewardDecision, StewardJob, StewardPolicy,
    StewardPullRequest, StewardRun, classify_pr, coalescing_reason_authorizes,
    has_successful_status, is_capacity_preemption_workflow, is_full_sha,
    is_safe_capacity_preemption, plan_capacity_preemptions, plan_run_coalescing, preemption_key,
    queue_front_waits_for_pool,
};
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::ship_state::{ShipState, ShipStateStore};

pub(super) struct StewardCommandArgs {
    pub(super) repos: Vec<String>,
    pub(super) base: String,
    pub(super) opt_out_label: String,
    pub(super) managed_label: String,
    pub(super) handoff_context: String,
    pub(super) max_transient_reruns: u32,
    pub(super) coalesce: bool,
    pub(super) preempt_capacity: bool,
    pub(super) max_preemptions_per_head: u32,
    pub(super) apply: bool,
    pub(super) ledger: Option<PathBuf>,
}

pub(super) struct AdmissionCleanArgs {
    pub(super) repo: String,
    pub(super) base: String,
    pub(super) labels: Vec<String>,
    pub(super) apply: bool,
}

#[derive(Clone, Debug)]
struct ObservedPr {
    node_id: String,
    fact: StewardPullRequest,
    check_rollup_maybe_truncated: bool,
}

#[derive(Clone, Debug)]
struct RepoObservation {
    repo: String,
    base: String,
    allow_auto_merge: bool,
    merge_queue: bool,
    required_checks: Vec<RequiredCheck>,
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
    #[serde(default = "default_managed_label")]
    managed_label: String,
    #[serde(default = "default_handoff_context")]
    handoff_context: String,
}

fn default_managed_label() -> String {
    MANAGED_LABEL.to_owned()
}

fn default_handoff_context() -> String {
    HANDOFF_CONTEXT.to_owned()
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
    managed_label: &'a str,
    handoff_context: &'a str,
}

const CANCEL_TERMINAL_WAIT: Duration = Duration::from_secs(15);
const CANCEL_TERMINAL_POLL: Duration = Duration::from_secs(2);
const PREEMPT_AFTER_SECS: i64 = 900;
pub(super) const HANDOFF_CONTEXT: &str = "shipyard/steward-handoff";
pub(super) const MANAGED_LABEL: &str = "shipyard:managed";
pub(super) const UNMANAGED_LABEL: &str = "shipyard:unmanaged";
pub(super) const RECOVERY_CONTEXT: &str = "shipyard/steward-recovery";
pub(super) const NEEDS_AGENT_LABEL: &str = "shipyard:needs-agent";

mod handoff;
pub(crate) use handoff::{
    StewardHandoffArgs, existing_handoff_receipt_is_valid, steward_handoff_command,
};
mod recovery;
use recovery::{reconcile_management_label, reconcile_recovery_signal};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdmissionVerdict {
    Admit,
    Defer,
    Error,
}

impl AdmissionVerdict {
    const fn label(self) -> &'static str {
        match self {
            Self::Admit => "admit",
            Self::Defer => "defer",
            Self::Error => "error",
        }
    }

    fn exit_code(self) -> ExitCode {
        match self {
            Self::Admit => ExitCode::SUCCESS,
            Self::Defer => ExitCode::from(3),
            Self::Error => ExitCode::from(1),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_admission_verdict<W: Write>(
    stdout: &mut W,
    json_output: bool,
    verdict: AdmissionVerdict,
    reason: &str,
    repo: &str,
    base: &str,
    labels: &[String],
    blocker_run_ids: &[u64],
) -> Result<ExitCode, CliFailure> {
    let payload = json!({
        "schema_version": 1,
        "command": "runner:admission-clean",
        "verdict": verdict.label(),
        "reason": reason,
        "repo": repo,
        "base": base,
        "labels": labels,
        "observed_at": Utc::now().to_rfc3339(),
        "blocker_run_ids": blocker_run_ids,
    });
    if json_output {
        serde_json::to_writer(&mut *stdout, &payload)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "{}: {reason}", verdict.label())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if !blocker_run_ids.is_empty() {
            writeln!(stdout, "blocker runs: {blocker_run_ids:?}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(verdict.exit_code())
}

fn normalize_admission_target(
    args: &AdmissionCleanArgs,
) -> Result<(String, String, Vec<String>), CliFailure> {
    let Some((owner, name)) = args.repo.split_once('/') else {
        return Err(CliFailure::new(2, "--repo must be an owner/repo slug"));
    };
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || !owner
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || value == '-')
        || !name
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '-'))
    {
        return Err(CliFailure::new(2, "--repo must be an owner/repo slug"));
    }
    if args.base.is_empty()
        || args.base.starts_with('/')
        || args.base.ends_with('/')
        || args.base.contains("..")
        || !args
            .base
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '-' | '/'))
    {
        return Err(CliFailure::new(2, "--base is not a valid branch name"));
    }
    if args.labels.is_empty() || args.labels.len() > 100 {
        return Err(CliFailure::new(
            2,
            "--labels must contain 1..100 comma-separated labels",
        ));
    }
    let mut labels = Vec::with_capacity(args.labels.len());
    let mut seen = BTreeSet::new();
    for raw in &args.labels {
        let label = raw.trim().to_ascii_lowercase();
        if label.is_empty() || label.len() > 100 || !seen.insert(label.clone()) {
            return Err(CliFailure::new(
                2,
                "--labels must be non-empty, unique, and at most 100 bytes each",
            ));
        }
        labels.push(label);
    }
    labels.sort();
    if !labels.iter().any(|label| label == "self-hosted") {
        return Err(CliFailure::new(
            2,
            "--labels must include the self-hosted label",
        ));
    }
    Ok((args.repo.clone(), args.base.clone(), labels))
}

fn job_can_claim_runner(job: &StewardJob, runner_labels: &BTreeSet<String>) -> bool {
    if !job.status.eq_ignore_ascii_case("queued") || job.labels.is_empty() {
        return false;
    }
    job.labels
        .iter()
        .all(|label| runner_labels.contains(&label.to_ascii_lowercase()))
}

fn admission_candidates(
    actions: &GitHubActions,
    observation: &RepoObservation,
    labels: &[String],
) -> Result<Vec<RunCancellation>, String> {
    let current_heads = current_pull_request_heads(&observation.prs);
    let mut excluded = opted_out_pull_requests(&observation.prs, "shipyard:no-auto-merge");
    excluded.extend(
        observation
            .prs
            .iter()
            .filter(|pr| !pull_request_is_managed(pr, MANAGED_LABEL, HANDOFF_CONTEXT))
            .map(|pr| pr.fact.number),
    );
    let candidates = plan_run_coalescing(
        &observation.runs,
        &current_heads,
        &observation.merge_group_heads,
        &excluded,
    );
    if candidates.len() > 50 {
        return Err("more than 50 superseded runs require admission inspection".to_owned());
    }
    let runner_labels = labels.iter().cloned().collect::<BTreeSet<_>>();
    let mut compatible = Vec::new();
    for candidate in candidates {
        let run = observation
            .runs
            .iter()
            .find(|run| run.id == candidate.run_id)
            .ok_or_else(|| format!("planned run {} disappeared", candidate.run_id))?;
        let jobs = fetch_run_jobs(actions, &observation.repo, run.id)?;
        if jobs
            .iter()
            .any(|job| job_can_claim_runner(job, &runner_labels))
        {
            compatible.push(candidate);
        }
    }
    compatible.sort_unstable_by_key(|candidate| candidate.run_id);
    Ok(compatible)
}

fn observe_admission_candidates(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    labels: &[String],
) -> Result<(RepoObservation, Vec<RunCancellation>), String> {
    let observation = observe_repo(actions, repo, base, false)?;
    let candidates = admission_candidates(actions, &observation, labels)?;
    Ok((observation, candidates))
}

fn pending_admission_cancellations(
    ledger: &StewardLedger,
    repo: &str,
    base: &str,
) -> Vec<(String, PendingCancellation)> {
    ledger
        .pending_cancellations
        .iter()
        .filter(|(_, pending)| pending.repo == repo && pending.base == base)
        .map(|(key, pending)| (key.clone(), pending.clone()))
        .collect()
}

fn admission_blocker_run_ids(
    candidates: &[RunCancellation],
    pending: &[(String, PendingCancellation)],
) -> Vec<u64> {
    let mut run_ids = candidates
        .iter()
        .map(|candidate| candidate.run_id)
        .chain(pending.iter().map(|(_, cancellation)| cancellation.run_id))
        .collect::<Vec<_>>();
    run_ids.sort_unstable();
    run_ids.dedup();
    run_ids
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn admission_clean_command<W: Write>(
    args: &AdmissionCleanArgs,
    cwd: &Path,
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let (repo, base, labels) = normalize_admission_target(args)?;
    let Ok((mut observation, mut candidates)) =
        observe_admission_candidates(actions, &repo, &base, &labels)
    else {
        return emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Error,
            "observation_failed",
            &repo,
            &base,
            &labels,
            &[],
        );
    };
    let canonical_repo = observation.repo.clone();
    let ledger_path = runtime_paths.state_dir.join("merge-steward.json");
    let Ok(observed_ledger) = load_ledger(&ledger_path) else {
        return emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Error,
            "mutation_failed",
            &canonical_repo,
            &base,
            &labels,
            &[],
        );
    };
    let observed_pending =
        pending_admission_cancellations(&observed_ledger, &canonical_repo, &base);
    let mut blocker_run_ids = admission_blocker_run_ids(&candidates, &observed_pending);
    if candidates.is_empty() && observed_pending.is_empty() {
        return emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Admit,
            "clean",
            &canonical_repo,
            &base,
            &labels,
            &[],
        );
    }
    if !args.apply {
        return emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Defer,
            if observed_pending.is_empty() {
                "stale_compatible_runs"
            } else {
                "cancellation_pending"
            },
            &canonical_repo,
            &base,
            &labels,
            &blocker_run_ids,
        );
    }
    let Ok(authority) = authority_status(
        &runtime_paths.state_dir,
        cwd,
        mode,
        &runtime_paths.global_dir,
    ) else {
        return emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Error,
            "authority_failed",
            &canonical_repo,
            &base,
            &labels,
            &blocker_run_ids,
        );
    };
    if authority.get("authority_matches").and_then(Value::as_bool) != Some(true) {
        return emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Defer,
            "mutation_authority_required",
            &canonical_repo,
            &base,
            &labels,
            &blocker_run_ids,
        );
    }

    let _ledger_lock = acquire_ledger_lock(&ledger_path)?;
    let mut ledger = load_ledger(&ledger_path)?;
    let pending = pending_admission_cancellations(&ledger, &canonical_repo, &base);
    blocker_run_ids = admission_blocker_run_ids(&candidates, &pending);
    if candidates.is_empty() && pending.is_empty() {
        return emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Admit,
            "cleaned",
            &canonical_repo,
            &base,
            &labels,
            &[],
        );
    }
    let mutation_control = MutationControl {
        store: ShipStateStore::new(runtime_paths.state_dir.join("ship")).map_err(|error| {
            CliFailure::new(
                1,
                format!("could not open merge-queue mutation state: {error}"),
            )
        })?,
        cwd: cwd.to_path_buf(),
        mode,
        global_dir: runtime_paths.global_dir.clone(),
    };
    for (key, pending) in pending {
        if cancellation_recovery::resume_pending_cancellation(
            actions,
            &ledger_path,
            &mut ledger,
            &mutation_control,
            &key,
            &pending,
        )
        .is_err()
        {
            return emit_admission_verdict(
                stdout,
                json_output,
                AdmissionVerdict::Error,
                "mutation_failed",
                &canonical_repo,
                &base,
                &labels,
                &blocker_run_ids,
            );
        }
    }
    match observe_admission_candidates(actions, &canonical_repo, &base, &labels) {
        Ok((refreshed_observation, refreshed_candidates)) => {
            observation = refreshed_observation;
            candidates = refreshed_candidates;
            let remaining_pending =
                pending_admission_cancellations(&ledger, &canonical_repo, &base);
            blocker_run_ids = admission_blocker_run_ids(&candidates, &remaining_pending);
        }
        Err(_) => {
            return emit_admission_verdict(
                stdout,
                json_output,
                AdmissionVerdict::Error,
                "revalidation_failed",
                &canonical_repo,
                &base,
                &labels,
                &blocker_run_ids,
            );
        }
    }
    if candidates.is_empty() {
        return emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Admit,
            "cleaned",
            &canonical_repo,
            &base,
            &labels,
            &[],
        );
    }
    for candidate in &candidates {
        let (_, error) = cancellation::apply_run_cancellation(
            actions,
            &observation,
            candidate,
            "shipyard:no-auto-merge",
            MANAGED_LABEL,
            HANDOFF_CONTEXT,
            &ledger_path,
            &mut ledger,
            &mutation_control,
        );
        if error.is_some() {
            let _ = save_ledger(&ledger_path, &ledger);
            return emit_admission_verdict(
                stdout,
                json_output,
                AdmissionVerdict::Error,
                "mutation_failed",
                &canonical_repo,
                &base,
                &labels,
                &blocker_run_ids,
            );
        }
    }
    save_ledger(&ledger_path, &ledger)?;
    let Ok((_, remaining)) = observe_admission_candidates(actions, &canonical_repo, &base, &labels)
    else {
        return emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Error,
            "revalidation_failed",
            &canonical_repo,
            &base,
            &labels,
            &blocker_run_ids,
        );
    };
    if remaining.is_empty() {
        emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Admit,
            "cleaned",
            &canonical_repo,
            &base,
            &labels,
            &[],
        )
    } else {
        let remaining = remaining
            .into_iter()
            .map(|candidate| candidate.run_id)
            .collect::<Vec<_>>();
        emit_admission_verdict(
            stdout,
            json_output,
            AdmissionVerdict::Defer,
            "cancellation_pending",
            &canonical_repo,
            &base,
            &labels,
            &remaining,
        )
    }
}

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
        match observe_repo(actions, &repo, &args.base, args.preempt_capacity) {
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
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
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
        let reason = if lock_is_contended(&error) {
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
#[cfg(all(test, unix))]
use cancellation_revalidation::pull_request;
use cancellation_revalidation::{
    acquire_pr_mutation_guard, acquire_run_mutation_guard, attempts_for,
    authoritative_head_still_superseded, current_pull_request_heads, exact_run_still_queued,
    merge_group_pr_number, opted_out_pull_requests, pull_request_is_managed,
    pull_request_with_required_checks, revalidate_capacity_preemption,
    revalidate_coalescing_cancellation,
};
use cancellation_terminalization::{
    acquire_pending_cancellation_guard, active_runner_targets, clear_pending_cancellation,
    complete_capacity_cancellation, finish_force_cancel_revalidation_failure,
    persist_force_cancel_intent, read_current_pending_run_identity, read_pending_run,
    validate_pending_cancellation_authority,
};
use capacity_cancellation::{
    CapacityCancelError, apply_capacity_preemption, cancel_capacity_preemption_after_revalidation,
    mark_cancellation_skipped, persist_capacity_evidence,
};
use ledger::{attempt_key, load_ledger, record_audit, save_ledger};
use observation::{
    active_runs, fetch_run_jobs, fetch_run_jobs_before, gh_json, gh_json_timeout,
    hydrate_required_check_identities, merge_queue_snapshot, observe_repo, parse_job, parse_pr,
    parse_run, pull_requests, resolve_repos,
};
use pr_mutations::mutate_pr;
use render::{
    enqueue_requirements_pending, is_admin_protection_denied, is_private_free_entitlement,
    render_report,
};

#[cfg(test)]
mod tests;
