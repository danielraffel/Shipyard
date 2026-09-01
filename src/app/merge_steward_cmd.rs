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
use sha2::{Digest, Sha256};

use super::CliFailure;
use crate::cloud::GitHubActions;
use crate::dispatch_wedge::{
    DispatchJobAuthority, DispatchRunnerObservation, DispatchWedgeObservation,
};
use crate::identity::RuntimeMode;
use crate::merge_queue_control::{
    DurableMutationIntent, MergeQueueMutationGuard, authority_status, lock_is_contended,
};
use crate::merge_steward::{
    CapacityPreemptionPolicy, QueueFrontPressure, RequiredCheck, RunCancellation,
    RunCancellationReason, StalePrRunWedgeCandidate, StewardCheck, StewardDecision, StewardJob,
    StewardPolicy, StewardPullRequest, StewardRun, classify_pr, coalescing_reason_authorizes,
    has_successful_status, is_capacity_preemption_workflow, is_full_sha,
    is_safe_capacity_preemption, plan_capacity_preemptions, plan_run_coalescing,
    plan_stale_pr_run_wedges, preemption_key, queue_front_waits_for_pool,
};
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::ship_state::{ShipState, ShipStateStore};

pub(super) mod recovery_worker;

#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)] // Flat flags mirror the steward CLI policy contract.
pub(super) struct StewardCommandArgs {
    pub(super) repos: Vec<String>,
    pub(super) base: String,
    pub(super) opt_out_label: String,
    pub(super) provenance_blocking_labels: Vec<String>,
    pub(super) managed_label: String,
    pub(super) handoff_context: String,
    pub(super) max_transient_reruns: u32,
    pub(super) recover_hosted_setup_eviction_priority: bool,
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
    stale_pr_run_wedge: StalePrRunWedgeRepoStatus,
    errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct StalePrRunWedgeRepoStatus {
    policy: String,
    candidates: Vec<StalePrRunWedgeCandidate>,
    receipts: Vec<StalePrRunWedgeReceipt>,
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
    stale_pr_run_wedge_receipts: BTreeMap<String, StalePrRunWedgeReceipt>,
    #[serde(default)]
    queue_witnesses: BTreeMap<String, QueueWitness>,
    #[serde(default)]
    queue_recovery_receipts: BTreeMap<String, QueueRecoveryReceipt>,
    #[serde(default)]
    terminal_handoffs: BTreeMap<String, TerminalHandoff>,
    #[serde(default)]
    resume_records: BTreeMap<String, resume_record::ResumeRecordV1>,
    #[serde(default)]
    audit: Vec<LedgerAudit>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct StalePrRunWedgeReceipt {
    candidate: StalePrRunWedgeCandidate,
    phase: StalePrRunWedgeReceiptPhase,
    mutation_correlation_id: String,
    created_at: String,
    updated_at: String,
    detail: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StalePrRunWedgeReceiptPhase {
    Intent,
    Accepted,
    Terminal,
    Skipped,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct TerminalHandoff {
    dedupe_key: String,
    repo: String,
    base: String,
    pr_number: u64,
    head_sha: String,
    outcome: TerminalHandoffOutcome,
    trigger: String,
    next_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin_machine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ownership_generation: Option<u64>,
    owner_disposition: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_route_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resume_transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_terminal_provenance: Option<TerminalProvenanceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_route: Option<handoff::ProviderRouteReferenceV1>,
    wake_consumer_available: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    failure_contexts: Vec<String>,
    phase: TerminalHandoffPhase,
    created_at: String,
    updated_at: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalHandoffOutcome {
    SuccessContinuation,
    ActionableFailure,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalHandoffPhase {
    Pending,
    Recorded,
    Applied,
    Resolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(unix)]
pub(crate) enum ExactStewardTransition {
    None,
    Actionable,
    Terminal,
}

/// Read the merge steward's authenticated, exact-head transition ledger. An
/// aggregate check-rollup failure is never itself agent-launch authority; only
/// the steward path that evaluated required-check policy may record this fact.
#[cfg(unix)]
pub(crate) fn exact_steward_transition(
    state_dir: &Path,
    repository_provider: Option<&str>,
    repository_id: Option<&str>,
    repo: &str,
    pr: u64,
    head_sha: &str,
) -> Result<ExactStewardTransition, String> {
    let (Some(repository_provider), Some(repository_id)) = (repository_provider, repository_id)
    else {
        return Err("exact steward transition lacks immutable repository identity".to_owned());
    };
    crate::work_ledger::verify_native_policy_binding_for_repository(
        state_dir,
        repository_provider,
        repository_id,
        repo,
        pr,
        head_sha,
    )
    .map_err(|error| error.to_string())?;
    let path = state_dir.join("merge-steward.json");
    let Some(ledger) = ledger::load_existing_ledger(&path).map_err(|error| error.message)? else {
        return Ok(ExactStewardTransition::None);
    };
    let exact = ledger.terminal_handoffs.values().filter(|handoff| {
        handoff.repo.eq_ignore_ascii_case(repo)
            && handoff.pr_number == pr
            && handoff.head_sha.eq_ignore_ascii_case(head_sha)
    });
    let mut saw_exact = false;
    let mut all_resolved = true;
    for handoff in exact {
        saw_exact = true;
        if handoff.outcome == TerminalHandoffOutcome::ActionableFailure
            && handoff.phase == TerminalHandoffPhase::Recorded
            && handoff.trigger == "actionable_terminal_failure"
            && handoff.next_action == "wake_exact_owner_for_causal_repair"
        {
            return Ok(ExactStewardTransition::Actionable);
        }
        all_resolved &= matches!(
            handoff.phase,
            TerminalHandoffPhase::Applied | TerminalHandoffPhase::Resolved
        );
    }
    Ok(if saw_exact && all_resolved {
        ExactStewardTransition::Terminal
    } else {
        ExactStewardTransition::None
    })
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalProvenanceKind {
    #[default]
    Absent,
    Cmux,
    HerdR,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct QueueWitness {
    repo: String,
    base: String,
    base_sha: String,
    pr_number: u64,
    pr_head: String,
    merge_group_head: String,
    position: u64,
    enqueued_at: String,
    observed_at: String,
    required_checks: Vec<WitnessRequiredCheck>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct WitnessRequiredCheck {
    context: String,
    app_id: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct QueueRecoveryReceipt {
    repo: String,
    base: String,
    pr_number: u64,
    pr_head: String,
    base_sha: String,
    merge_group_head: String,
    removed_at: String,
    run_id: u64,
    job_id: u64,
    attempted_at: String,
    phase: QueueRecoveryPhase,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum QueueRecoveryPhase {
    Intent,
    Accepted,
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
    #[serde(default = "default_provenance_blocking_labels")]
    provenance_blocking_labels: Vec<String>,
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

fn default_provenance_blocking_labels() -> Vec<String> {
    vec![DEFAULT_PROVENANCE_BLOCKING_LABEL.to_owned()]
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
    state_dir: PathBuf,
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
    provenance_blocking_labels: &'a [String],
}

const CANCEL_TERMINAL_WAIT: Duration = Duration::from_secs(15);
const CANCEL_TERMINAL_POLL: Duration = Duration::from_secs(2);
const ADMISSION_OBSERVATION_TIMEOUT: Duration = Duration::from_mins(2);
const PREEMPT_AFTER_SECS: i64 = 900;
pub(super) const HANDOFF_CONTEXT: &str = "shipyard/steward-handoff";
pub(super) const MANAGED_LABEL: &str = "shipyard:managed";
pub(super) const DEFAULT_PROVENANCE_BLOCKING_LABEL: &str = "5·unresolved";
pub(super) const UNMANAGED_LABEL: &str = "shipyard:unmanaged";
pub(super) const RECOVERY_CONTEXT: &str = "shipyard/steward-recovery";
pub(super) const NEEDS_AGENT_LABEL: &str = "shipyard:needs-agent";

mod handoff;
#[cfg(test)]
pub(crate) use handoff::steward_handoff_command_without_ambient;
#[cfg(unix)]
pub(crate) use handoff::verify_native_repository_identity;
pub(crate) use handoff::{
    StewardHandoffArgs, native_publication_request, steward_handoff_command,
    steward_handoff_transfer_report,
};
mod launch_profile;
#[allow(unused_imports)] // Consumed by the daemon wake-loop integration slice.
pub(crate) use launch_profile::{LaunchProfileV1, decode_protected_launch_profile};
mod recovery;
use recovery::{reconcile_management_label, reconcile_recovery_signal};
mod stale_pr_wedge;

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
    error_detail: Option<&str>,
) -> Result<ExitCode, CliFailure> {
    let mut payload = json!({
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
    if let Some(error) = error_detail {
        payload["error"] = Value::String(bounded_admission_error(error));
    }
    if json_output {
        serde_json::to_writer(&mut *stdout, &payload)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "{}: {reason}", verdict.label())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if let Some(error) = error_detail {
            writeln!(stdout, "error: {}", bounded_admission_error(error))
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        if !blocker_run_ids.is_empty() {
            writeln!(stdout, "blocker runs: {blocker_run_ids:?}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(verdict.exit_code())
}

fn bounded_admission_error(error: &str) -> String {
    const MAX_BYTES: usize = 4 * 1024;
    if error.len() <= MAX_BYTES {
        return error.to_owned();
    }
    let prefix = &error[..error.floor_char_boundary(MAX_BYTES)];
    format!("{prefix}...[truncated]")
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

fn try_acquire_admission_observation_lock(
    state_dir: &Path,
    repo: &str,
    base: &str,
    labels: &[String],
) -> Result<Option<fs::File>, String> {
    let directory = state_dir.join("runner-admission-observation");
    crate::writer_domain_lease::ensure_protected_dir_all(&directory)
        .map_err(|error| format!("could not create admission observation state: {error}"))?;
    let key = serde_json::to_vec(&(repo.to_ascii_lowercase(), base, labels))
        .map_err(|error| format!("could not encode admission observation key: {error}"))?;
    let path = directory.join(format!("{:x}.lock", Sha256::digest(key)));
    let writer_domain = crate::writer_domain_lease::acquire_for_protected_creation(&path)
        .map_err(|error| format!("could not fence admission observation lock: {error}"))?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    drop(writer_domain);
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if lock_is_contended(&error) => Ok(None),
        Err(error) => Err(format!(
            "could not lock admission observation state: {error}"
        )),
    }
}

fn admission_candidates(
    actions: &GitHubActions,
    observation: &RepoObservation,
    labels: &[String],
) -> Result<(Vec<RunCancellation>, Vec<u64>), String> {
    let (current_heads, excluded) = admission_pr_authority(&observation.prs);
    let cancellable = plan_run_coalescing(
        &observation.runs,
        &current_heads,
        &observation.merge_group_heads,
        &excluded,
    );
    let mut in_progress = observation
        .runs
        .iter()
        .filter(|run| run.status.eq_ignore_ascii_case("in_progress"))
        .cloned()
        .collect::<Vec<_>>();
    for run in &mut in_progress {
        "queued".clone_into(&mut run.status);
    }
    let blocking_only = plan_run_coalescing(
        &in_progress,
        &current_heads,
        &observation.merge_group_heads,
        &excluded,
    );
    if cancellable.len() + blocking_only.len() > 50 {
        return Err("more than 50 superseded runs require admission inspection".to_owned());
    }
    let runner_labels = labels.iter().cloned().collect::<BTreeSet<_>>();
    let mut compatible = Vec::new();
    let mut blockers = Vec::new();
    let mut candidates = cancellable;
    candidates.extend(blocking_only);
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
            if run.status.eq_ignore_ascii_case("queued") {
                compatible.push(candidate);
            } else {
                blockers.push(candidate.run_id);
            }
        }
    }
    compatible.sort_unstable_by_key(|candidate| candidate.run_id);
    Ok((compatible, blockers))
}

fn admission_pr_authority(prs: &[ObservedPr]) -> (BTreeMap<u64, String>, BTreeSet<u64>) {
    let current_heads = current_pull_request_heads(prs);
    let mut excluded = opted_out_pull_requests(prs, "shipyard:no-auto-merge");
    excluded.extend(
        prs.iter()
            .filter(|pr| !pull_request_is_managed(pr, MANAGED_LABEL, HANDOFF_CONTEXT))
            .map(|pr| pr.fact.number),
    );
    (current_heads, excluded)
}

fn observe_admission_candidates(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    labels: &[String],
) -> Result<(RepoObservation, Vec<RunCancellation>, Vec<u64>), String> {
    let observation = observe_repo(actions, repo, base, false)?;
    let (candidates, blocking_only) = admission_candidates(actions, &observation, labels)?;
    fence_admission_authority(actions, &observation)?;
    Ok((observation, candidates, blocking_only))
}

fn refreshed_admission_authority(
    actions: &GitHubActions,
    observation: &RepoObservation,
) -> Result<(Vec<ObservedPr>, BTreeMap<u64, String>), String> {
    let (_, positions, heads, _) =
        merge_queue_snapshot(actions, &observation.repo, &observation.base)?;
    let mut prs = pull_requests(actions, &observation.repo, &observation.base, &positions)?;
    hydrate_required_check_identities(actions, &observation.repo, &[], &mut prs)?;
    Ok((prs, heads))
}

fn fence_admission_authority(
    actions: &GitHubActions,
    observation: &RepoObservation,
) -> Result<(), String> {
    let (final_prs, final_merge_group_heads) = refreshed_admission_authority(actions, observation)?;
    if admission_pr_authority(&observation.prs) != admission_pr_authority(&final_prs)
        || observation.merge_group_heads != final_merge_group_heads
    {
        return Err("admission authority changed during active-run inspection".to_owned());
    }
    Ok(())
}

fn revalidate_active_admission_candidates(
    actions: &GitHubActions,
    observation: &RepoObservation,
    labels: &[String],
) -> Result<(Vec<RunCancellation>, Vec<u64>), String> {
    let mut refreshed = observation.clone();
    let (prs, heads) = refreshed_admission_authority(actions, observation)?;
    refreshed.prs = prs;
    refreshed.merge_group_heads = heads;
    refreshed.runs = active_runs(actions, &observation.repo)?;
    let candidates = admission_candidates(actions, &refreshed, labels)?;
    fence_admission_authority(actions, &refreshed)?;
    Ok(candidates)
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
    blocking_only: &[u64],
    pending: &[(String, PendingCancellation)],
) -> Vec<u64> {
    let mut run_ids = candidates
        .iter()
        .map(|candidate| candidate.run_id)
        .chain(blocking_only.iter().copied())
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
    macro_rules! return_verdict {
        ($verdict:ident, $reason:expr, $repo:expr, $blockers:expr, $error:expr) => {
            return emit_admission_verdict(
                stdout,
                json_output,
                AdmissionVerdict::$verdict,
                $reason,
                $repo,
                &base,
                &labels,
                $blockers,
                $error,
            )
        };
    }
    let _observation_lock = match try_acquire_admission_observation_lock(
        &runtime_paths.state_dir,
        &repo,
        &base,
        &labels,
    ) {
        Ok(Some(lock)) => lock,
        Ok(None) => return_verdict!(Defer, "observation_in_progress", &repo, &[], None),
        Err(error) => return_verdict!(Error, "observation_failed", &repo, &[], Some(&error)),
    };
    let admission_actions = actions
        .clone()
        .with_absolute_deadline(Instant::now() + ADMISSION_OBSERVATION_TIMEOUT);
    let actions = &admission_actions;
    let (mut observation, mut candidates, mut blocking_only) =
        match observe_admission_candidates(actions, &repo, &base, &labels) {
            Ok(observation) => observation,
            Err(error) => return_verdict!(Error, "observation_failed", &repo, &[], Some(&error)),
        };
    let canonical_repo = observation.repo.clone();
    let ledger_path = runtime_paths.state_dir.join("merge-steward.json");
    macro_rules! acquire_admission_ledger_lock {
        ($blockers:expr) => {
            match try_acquire_ledger_lock(&ledger_path) {
                Ok(Some(lock)) => lock,
                Ok(None) => return_verdict!(
                    Defer,
                    "stewardship_in_progress",
                    &canonical_repo,
                    $blockers,
                    Some("another steward apply pass is already running")
                ),
                Err(error) => return_verdict!(
                    Error,
                    "mutation_failed",
                    &canonical_repo,
                    $blockers,
                    Some(error.message())
                ),
            }
        };
    }
    let Ok(observed_ledger) = load_ledger(&ledger_path) else {
        return_verdict!(Error, "mutation_failed", &canonical_repo, &[], None);
    };
    let observed_pending =
        pending_admission_cancellations(&observed_ledger, &canonical_repo, &base);
    let mut blocker_run_ids =
        admission_blocker_run_ids(&candidates, &blocking_only, &observed_pending);
    if candidates.is_empty() && blocking_only.is_empty() && observed_pending.is_empty() {
        let _ledger_lock = acquire_admission_ledger_lock!(&[]);
        let ledger = load_ledger(&ledger_path)?;
        let final_pending = pending_admission_cancellations(&ledger, &canonical_repo, &base);
        if !final_pending.is_empty() {
            let blockers = admission_blocker_run_ids(&[], &[], &final_pending);
            return_verdict!(
                Defer,
                "cancellation_pending",
                &canonical_repo,
                &blockers,
                None
            );
        }
        let (final_candidates, final_blocking) =
            match revalidate_active_admission_candidates(actions, &observation, &labels) {
                Ok(candidates) => candidates,
                Err(error) => return_verdict!(
                    Error,
                    "revalidation_failed",
                    &canonical_repo,
                    &[],
                    Some(&error)
                ),
            };
        if !final_candidates.is_empty() || !final_blocking.is_empty() {
            let blockers = admission_blocker_run_ids(&final_candidates, &final_blocking, &[]);
            return_verdict!(
                Defer,
                "stale_compatible_runs",
                &canonical_repo,
                &blockers,
                None
            );
        }
        return_verdict!(Admit, "clean", &canonical_repo, &[], None);
    }
    if !args.apply {
        return_verdict!(
            Defer,
            if observed_pending.is_empty() {
                "stale_compatible_runs"
            } else {
                "cancellation_pending"
            },
            &canonical_repo,
            &blocker_run_ids,
            None
        );
    }
    let Ok(authority) = authority_status(
        &runtime_paths.state_dir,
        cwd,
        mode,
        &runtime_paths.global_dir,
    ) else {
        return_verdict!(
            Error,
            "authority_failed",
            &canonical_repo,
            &blocker_run_ids,
            None
        );
    };
    if authority.get("authority_matches").and_then(Value::as_bool) != Some(true) {
        return_verdict!(
            Defer,
            "mutation_authority_required",
            &canonical_repo,
            &blocker_run_ids,
            None
        );
    }

    let _ledger_lock = acquire_admission_ledger_lock!(&blocker_run_ids);
    let mut ledger = load_ledger(&ledger_path)?;
    let pending = pending_admission_cancellations(&ledger, &canonical_repo, &base);
    blocker_run_ids = admission_blocker_run_ids(&candidates, &blocking_only, &pending);
    // Reobserve all pre-lock blockers before deciding whether to admit or defer.
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
        state_dir: runtime_paths.state_dir.clone(),
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
            return_verdict!(
                Error,
                "mutation_failed",
                &canonical_repo,
                &blocker_run_ids,
                None
            );
        }
    }
    match observe_admission_candidates(actions, &canonical_repo, &base, &labels) {
        Ok((refreshed_observation, refreshed_candidates, refreshed_blocking)) => {
            observation = refreshed_observation;
            candidates = refreshed_candidates;
            blocking_only = refreshed_blocking;
            let remaining_pending =
                pending_admission_cancellations(&ledger, &canonical_repo, &base);
            blocker_run_ids =
                admission_blocker_run_ids(&candidates, &blocking_only, &remaining_pending);
        }
        Err(error) => return_verdict!(
            Error,
            "revalidation_failed",
            &canonical_repo,
            &blocker_run_ids,
            Some(&error)
        ),
    }
    if candidates.is_empty() && blocking_only.is_empty() {
        return_verdict!(Admit, "cleaned", &canonical_repo, &[], None);
    }
    if candidates.is_empty() {
        return_verdict!(
            Defer,
            "stale_compatible_runs",
            &canonical_repo,
            &blocker_run_ids,
            None
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
            &default_provenance_blocking_labels(),
            &ledger_path,
            &mut ledger,
            &mutation_control,
        );
        if let Some(error) = error {
            let _ = save_ledger(&ledger_path, &ledger);
            return_verdict!(
                Error,
                "mutation_failed",
                &canonical_repo,
                &blocker_run_ids,
                Some(&error)
            );
        }
    }
    save_ledger(&ledger_path, &ledger)?;
    let (remaining, remaining_blocking) =
        match observe_admission_candidates(actions, &canonical_repo, &base, &labels) {
            Ok((_, candidates, blocking)) => (candidates, blocking),
            Err(error) => return_verdict!(
                Error,
                "revalidation_failed",
                &canonical_repo,
                &blocker_run_ids,
                Some(&error)
            ),
        };
    if remaining.is_empty() && remaining_blocking.is_empty() {
        return_verdict!(Admit, "cleaned", &canonical_repo, &[], None);
    }
    let only_blocking = remaining.is_empty();
    let mut remaining = remaining
        .into_iter()
        .map(|candidate| candidate.run_id)
        .collect::<Vec<_>>();
    remaining.extend(remaining_blocking);
    remaining.sort_unstable();
    remaining.dedup();
    return_verdict!(
        Defer,
        if only_blocking {
            "stale_compatible_runs"
        } else {
            "cancellation_pending"
        },
        &canonical_repo,
        &remaining,
        None
    );
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
    let (normalized_args, repos) = normalized_steward_args(args, cwd)?;
    let args = &normalized_args;
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
            state_dir: runtime_paths.state_dir.clone(),
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
                reports.push(unreadable_repo_report(&repo, &args.base, &ledger, error));
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

/// Read the exact queued merge-group jobs for one existing `WorkLedger` target.
/// This deliberately performs no mutation. Registered runner capacity is read
/// from GitHub; local admission holds remain a separate JIT/VM authority.
pub(crate) fn observe_dispatch_wedge_target(
    actions: &GitHubActions,
    repository: &str,
    base_ref: &str,
    pull_request: u64,
    expected_head_sha: &str,
) -> Result<Vec<DispatchWedgeObservation>, String> {
    let target = DispatchWedgeTargetRequest {
        base_ref: base_ref.to_owned(),
        pull_request,
        expected_head_sha: expected_head_sha.to_owned(),
    };
    let mut results = observe_dispatch_wedge_targets(actions, repository, &[target]);
    results
        .pop()
        .ok_or_else(|| "dispatch-wedge batch omitted its only target".to_owned())?
        .result
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DispatchWedgeTargetRequest {
    pub(crate) base_ref: String,
    pub(crate) pull_request: u64,
    pub(crate) expected_head_sha: String,
}

#[derive(Debug)]
pub(crate) struct DispatchWedgeTargetResult {
    pub(crate) target: DispatchWedgeTargetRequest,
    pub(crate) result: Result<Vec<DispatchWedgeObservation>, String>,
}

/// Observe every due target for one immutable repository identity in a single
/// bounded cycle. Repository policy/queue/run state is shared per exact base
/// and the registered-runner inventory is shared for the whole repository;
/// merge-head, run-attempt, job-detail, and producer identity remain exact to
/// each target. There is deliberately no cache across cycles.
pub(crate) fn observe_dispatch_wedge_targets(
    actions: &GitHubActions,
    repository: &str,
    targets: &[DispatchWedgeTargetRequest],
) -> Vec<DispatchWedgeTargetResult> {
    observe_dispatch_wedge_targets_with(
        targets,
        || dispatch_runner_observations(actions, repository),
        |base_ref| observe_repo(actions, repository, base_ref, false),
        |observation, target, runners| {
            observe_dispatch_wedge_target_from_repository(actions, observation, target, runners)
        },
    )
}

fn observe_dispatch_wedge_targets_with<Shared, LoadRunners, LoadRepository, ObserveTarget>(
    targets: &[DispatchWedgeTargetRequest],
    mut load_runners: LoadRunners,
    mut load_repository: LoadRepository,
    mut observe_target: ObserveTarget,
) -> Vec<DispatchWedgeTargetResult>
where
    LoadRunners: FnMut() -> Result<Vec<DispatchRunnerObservation>, String>,
    LoadRepository: FnMut(&str) -> Result<Shared, String>,
    ObserveTarget: FnMut(
        &Shared,
        &DispatchWedgeTargetRequest,
        &[DispatchRunnerObservation],
    ) -> Result<Vec<DispatchWedgeObservation>, String>,
{
    let runners = load_runners();
    let mut observations_by_base = BTreeMap::new();
    for target in targets {
        observations_by_base
            .entry(target.base_ref.clone())
            .or_insert_with(|| load_repository(&target.base_ref));
    }

    targets
        .iter()
        .cloned()
        .map(|target| {
            let result = match (&runners, observations_by_base.get(&target.base_ref)) {
                (Err(error), _) | (_, Some(Err(error))) => Err(error.clone()),
                (Ok(runners), Some(Ok(observation))) => {
                    observe_target(observation, &target, runners)
                }
                (_, None) => Err("dispatch-wedge repository observation was omitted".to_owned()),
            };
            DispatchWedgeTargetResult { target, result }
        })
        .collect()
}

fn observe_dispatch_wedge_target_from_repository(
    actions: &GitHubActions,
    observation: &RepoObservation,
    target: &DispatchWedgeTargetRequest,
    runners: &[DispatchRunnerObservation],
) -> Result<Vec<DispatchWedgeObservation>, String> {
    let pull_request = target.pull_request;
    let expected_head_sha = &target.expected_head_sha;
    let Some(pr) = observation.prs.iter().find(|candidate| {
        candidate.fact.number == pull_request
            && candidate
                .fact
                .head_sha
                .eq_ignore_ascii_case(expected_head_sha)
    }) else {
        return Ok(Vec::new());
    };
    let Some(queue_position) = pr.fact.queue_position else {
        return Ok(Vec::new());
    };
    let Some(merge_group_head) = observation.merge_group_heads.get(&pull_request) else {
        return Ok(Vec::new());
    };
    let mut results = collect_dispatch_wedge_job_observations(
        actions,
        observation,
        pr,
        queue_position,
        merge_group_head,
        runners,
    )?;
    if !results.is_empty() {
        revalidate_dispatch_wedge_authority_with(
            pr,
            target,
            queue_position,
            merge_group_head,
            || {
                cancellation_revalidation::pull_request(
                    actions,
                    &observation.repo,
                    pull_request,
                    &observation.base,
                    &BTreeMap::new(),
                )
            },
            || merge_queue_snapshot(actions, &observation.repo, &observation.base),
        )?;
        for result in &mut results {
            result.observation_complete = true;
        }
    }
    Ok(results)
}

fn collect_dispatch_wedge_job_observations(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    queue_position: u64,
    merge_group_head: &str,
    runners: &[DispatchRunnerObservation],
) -> Result<Vec<DispatchWedgeObservation>, String> {
    let check_producers =
        observation::job_check_producers_for_head(actions, &observation.repo, merge_group_head)?;
    let mut results = Vec::new();
    for run in observation.runs.iter().filter(|run| {
        run.event.eq_ignore_ascii_case("merge_group")
            && run.head_sha.eq_ignore_ascii_case(merge_group_head)
    }) {
        for job in observation::fetch_run_attempt_jobs(
            actions,
            &observation.repo,
            run.id,
            run.run_attempt,
        )? {
            if !job.status.eq_ignore_ascii_case("queued")
                || job.conclusion.is_some()
                || job
                    .runner_name
                    .as_deref()
                    .is_some_and(|name| !name.is_empty())
                || !observation
                    .required_checks
                    .iter()
                    .any(|required| required.context.eq_ignore_ascii_case(&job.name))
            {
                continue;
            }
            let detail = observation::gh_json(
                actions,
                &[
                    "api".to_owned(),
                    format!("repos/{}/actions/jobs/{}", observation.repo, job.id),
                ],
                "queued workflow job detail",
            )?;
            let detail_job = observation::parse_job(&detail)?;
            let Some(required) = current_required_dispatch_job(
                &job,
                &detail_job,
                run.id,
                &observation.required_checks,
                &check_producers,
            ) else {
                continue;
            };
            let producer_app_id = check_producers
                .get(&detail_job.id)
                .and_then(|check| check.app_id);
            results.push(DispatchWedgeObservation {
                authority: DispatchJobAuthority {
                    repository: observation.repo.clone(),
                    base_ref: observation.base.clone(),
                    pull_request: pr.fact.number,
                    pull_request_head: pr.fact.head_sha.clone(),
                    queue_position,
                    merge_group_head: merge_group_head.to_owned(),
                    workflow_run_id: run.id,
                    workflow_id: run.workflow_id,
                    run_attempt: run.run_attempt,
                    run_event: run.event.clone(),
                    run_head: run.head_sha.clone(),
                    job_id: detail_job.id,
                    job_name: detail_job.name.clone(),
                    job_status: detail_job.status.clone(),
                    job_conclusion: detail_job.conclusion.clone(),
                    runner_name: detail_job.runner_name.clone(),
                    labels: detail_job.labels.clone(),
                    // The durable producer replaces this observation-local
                    // value with the persisted first-seen time for this exact
                    // job before classification.
                    first_observed_unassigned_at: Utc::now().to_rfc3339(),
                    required_context: detail_job.name,
                    required_app_id: required.app_id,
                    producer_app_id,
                },
                runners: runners.to_vec(),
                observation_complete: false,
                #[cfg(test)]
                first_observed_unassigned_at_seed: None,
            });
        }
    }
    Ok(results)
}

fn revalidate_dispatch_wedge_authority_with<ReadPullRequest, ReadQueue>(
    observed_pr: &ObservedPr,
    target: &DispatchWedgeTargetRequest,
    observed_queue_position: u64,
    observed_merge_group_head: &str,
    mut read_pull_request: ReadPullRequest,
    mut read_queue: ReadQueue,
) -> Result<(), String>
where
    ReadPullRequest: FnMut() -> Result<Option<ObservedPr>, String>,
    ReadQueue: FnMut() -> Result<MergeQueueSnapshot, String>,
{
    let live_pr = read_pull_request()?
        .ok_or_else(|| "dispatch-wedge PR authority changed before final read".to_owned())?;
    if live_pr.fact.number != target.pull_request
        || !live_pr
            .fact
            .head_sha
            .eq_ignore_ascii_case(&target.expected_head_sha)
        || !live_pr
            .fact
            .head_sha
            .eq_ignore_ascii_case(&observed_pr.fact.head_sha)
    {
        return Err("dispatch-wedge PR authority changed before final read".to_owned());
    }

    // This is deliberately the last remote read before an observation becomes
    // complete. A dequeue, reinsertion, or regenerated merge group therefore
    // refuses the stale observation instead of publishing a wake from it.
    let (enabled, positions, heads, _) = read_queue()?;
    let live_position = positions.get(&target.pull_request).copied();
    let live_merge_group_head = heads.get(&target.pull_request);
    if !enabled
        || live_position != Some(observed_queue_position)
        || !live_merge_group_head
            .is_some_and(|head| head.eq_ignore_ascii_case(observed_merge_group_head))
    {
        return Err("dispatch-wedge queue authority changed before final read".to_owned());
    }
    Ok(())
}

fn current_required_dispatch_job<'a>(
    listed: &StewardJob,
    detail: &StewardJob,
    run_id: u64,
    required_checks: &'a [RequiredCheck],
    check_producers: &BTreeMap<u64, observation::JobCheckProducer>,
) -> Option<&'a RequiredCheck> {
    if detail.id != listed.id
        || !detail.name.eq_ignore_ascii_case(&listed.name)
        || !detail.status.eq_ignore_ascii_case("queued")
        || detail.conclusion.is_some()
        || detail
            .runner_name
            .as_deref()
            .is_some_and(|name| !name.is_empty())
    {
        return None;
    }
    let producer = check_producers.get(&detail.id);
    let mut matching = required_checks.iter().filter(|required| {
        required.context.eq_ignore_ascii_case(&detail.name)
            && required.app_id.is_none_or(|app_id| {
                producer.is_some_and(|producer| {
                    producer.run_id == run_id
                        && producer.job_id == detail.id
                        && producer.name.eq_ignore_ascii_case(&detail.name)
                        && producer.app_id == Some(app_id)
                })
            })
    });
    let required = matching.next()?;
    matching.next().is_none().then_some(required)
}

fn dispatch_runner_observations(
    actions: &GitHubActions,
    repository: &str,
) -> Result<Vec<DispatchRunnerObservation>, String> {
    let mut runners = Vec::new();
    for page in 1..=10 {
        let value = observation::gh_json(
            actions,
            &[
                "api".to_owned(),
                format!("repos/{repository}/actions/runners?per_page=100&page={page}"),
            ],
            "repository runner inventory",
        )?;
        let rows = value
            .get("runners")
            .and_then(Value::as_array)
            .ok_or_else(|| "repository runner inventory missing runners".to_owned())?;
        let count = rows.len();
        for row in rows {
            runners.push(DispatchRunnerObservation {
                runner_id: row.get("id").and_then(Value::as_u64).unwrap_or(0),
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
                busy: row.get("busy").and_then(Value::as_bool).unwrap_or(true),
                labels: row
                    .get("labels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|label| label.get("name").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect(),
            });
        }
        if count < 100 {
            return Ok(runners);
        }
    }
    Err("repository runner inventory exceeds 1000; refusing partial scan".to_owned())
}

fn unreadable_repo_report(
    repo: &str,
    base: &str,
    ledger: &StewardLedger,
    error: String,
) -> RepoReport {
    RepoReport {
        repo: repo.to_owned(),
        base: base.to_owned(),
        allow_auto_merge: false,
        merge_queue: false,
        merge_path: "unreadable".to_owned(),
        required_contexts: Vec::new(),
        prs: Vec::new(),
        cancellations: Vec::new(),
        stale_pr_run_wedge: stale_pr_wedge::repo_status(None, Vec::new(), ledger, repo),
        errors: vec![error],
    }
}

fn normalized_steward_args(
    args: &StewardCommandArgs,
    cwd: &Path,
) -> Result<(StewardCommandArgs, Vec<String>), CliFailure> {
    let mut normalized = args.clone();
    normalized.provenance_blocking_labels =
        normalize_provenance_blocking_labels(&args.provenance_blocking_labels)?;
    let repos = resolve_repos(normalized.repos.clone(), cwd)?;
    Ok((normalized, repos))
}

fn normalize_provenance_blocking_labels(labels: &[String]) -> Result<Vec<String>, CliFailure> {
    if labels.is_empty() || labels.len() > 100 {
        return Err(CliFailure::new(
            2,
            "--provenance-blocking-label must contain 1..100 labels",
        ));
    }
    let mut normalized = Vec::with_capacity(labels.len());
    let mut seen = BTreeSet::new();
    for raw in labels {
        let label = raw.trim();
        let identity = label.to_ascii_lowercase();
        if label.is_empty() || label.len() > 100 || !seen.insert(identity) {
            return Err(CliFailure::new(
                2,
                "--provenance-blocking-label values must be non-empty, unique, and at most 100 bytes each",
            ));
        }
        normalized.push(label.to_owned());
    }
    Ok(normalized)
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
            repo: repo.clone(),
            base,
            allow_auto_merge: false,
            merge_queue: false,
            merge_path: "pending_cancellation_recovery".to_owned(),
            required_contexts: Vec::new(),
            prs: Vec::new(),
            cancellations,
            stale_pr_run_wedge: stale_pr_wedge::repo_status(None, Vec::new(), ledger, &repo),
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
            stale_pr_run_wedge: stale_pr_wedge::repo_status(None, Vec::new(), ledger, "steward"),
            errors: vec![message],
        });
    }
}

fn try_acquire_ledger_lock(path: &Path) -> Result<Option<fs::File>, CliFailure> {
    let lock_path = path.with_extension("json.lock");
    let writer_domain = crate::writer_domain_lease::acquire_for_protected_creation(&lock_path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
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
    drop(writer_domain);
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if lock_is_contended(&error) => Ok(None),
        Err(error) => Err(CliFailure::new(
            1,
            format!(
                "could not lock steward state {}: {error}",
                lock_path.display()
            ),
        )),
    }
}

fn acquire_ledger_lock(path: &Path) -> Result<fs::File, CliFailure> {
    try_acquire_ledger_lock(path)?.ok_or_else(|| {
        CliFailure::new(
            1,
            format!(
                "could not lock steward state {}: another steward apply pass is already running",
                path.with_extension("json.lock").display()
            ),
        )
    })
}

mod cancellation;
mod cancellation_recovery;
mod cancellation_revalidation;
mod cancellation_terminalization;
mod capacity_cancellation;
mod ledger;
mod observation;
mod pr_mutations;
mod queue_priority_recovery;
mod render;
mod resume_record;
mod terminal_handoff;

use cancellation::{
    apply_repo_plan, cancellation_reason_label, queue_front_head, timestamp_old_enough,
};
use cancellation_recovery::resume_pending_cancellations;
use cancellation_revalidation::pull_request;
use cancellation_revalidation::{
    acquire_pr_mutation_guard, acquire_run_mutation_guard,
    acquire_run_mutation_guard_with_correlation, attempts_for, authoritative_head_still_superseded,
    current_pull_request_heads, exact_run_still_queued, merge_group_pr_number,
    opted_out_pull_requests, pull_request_is_managed, pull_request_opted_out,
    pull_request_provenance_blocked, pull_request_with_required_checks,
    pull_request_with_required_checks_before, revalidate_capacity_preemption,
    revalidate_coalescing_cancellation, revalidate_pending_pr_authority, run_mutation_state,
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
    active_runs, complete_checks_for_head, fetch_run_jobs, fetch_run_jobs_before, gh_json,
    gh_json_before, gh_json_timeout, hydrate_required_check_identities,
    hydrate_required_check_identities_before, merge_queue_snapshot, merge_queue_snapshot_before,
    observe_repo, parse_job, parse_pr, parse_run, pull_requests, required_checks, resolve_repos,
};
#[cfg(test)]
use pr_mutations::mutate_pr;
use render::{
    enqueue_requirements_pending, is_admin_protection_denied, is_private_free_entitlement,
    render_report,
};

#[cfg(test)]
mod tests;
