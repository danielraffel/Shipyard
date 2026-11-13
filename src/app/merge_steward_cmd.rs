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
use crate::identity::RuntimeMode;
use crate::merge_queue_control::{
    DurableMutationIntent, MergeQueueMutationGuard, authority_status, lock_is_contended,
};
use crate::merge_steward::{
    CapacityPreemptionPolicy, QueueFrontPressure, RequiredCheck, RunCancellation,
    RunCancellationReason, StalePrRunWedgeCandidate, StewardCheck, StewardDecision, StewardJob,
    StewardPolicy, StewardPullRequest, StewardRun, classify_pr, coalescing_reason_authorizes,
    has_successful_status, is_capacity_preemption_workflow, is_full_sha, is_implicitly_managed,
    is_safe_capacity_preemption, plan_capacity_preemptions, plan_run_coalescing,
    plan_stale_pr_run_wedges, preemption_key, queue_front_waits_for_pool,
};
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::ship_state::{ShipState, ShipStateStore};

pub(super) mod carrier;
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
    /// Also arm GitHub-native auto-merge on green, unqueued, unarmed pull
    /// requests the steward declines to own. See `native_arm`.
    pub(super) arm_unqueued: bool,
    /// Required check that reports exact-head approval; with it required
    /// and no merge queue, owned PRs are admitted by exact-head auto-merge.
    pub(super) approval_context: String,
    pub(super) wake_owner: bool,
    /// `[merge_steward] unapproved_dequeue`: whether the sweep removes
    /// unapproved queue entries or only reports them. Read from the
    /// machine-global config by [`steward_command`].
    pub(super) unapproved_dequeue: unapproved_admission::DequeueMode,
    /// `--infra-rerun`: re-run required checks that failed for an
    /// infrastructure reason, on every open pull request. See `infra_rerun`.
    pub(super) infra_rerun: infra_rerun::Mode,
    /// Each repository's time budget (`--pass-budget-secs`); its clock
    /// starts when that repository's observation starts, so a later
    /// repository is never starved by an earlier one. `None` never defers.
    pub(super) pass_budget: Option<Duration>,
    /// Past this instant the current repository starts no new per-PR work;
    /// see `pass_budget`. Set per repository from `pass_budget`.
    pub(super) pass_deadline: Option<Instant>,
    pub(super) apply: bool,
    pub(super) ledger: Option<PathBuf>,
    /// Tick-start invariants ([`invariants`]); `None` is `--no-invariants`.
    /// [`steward_command`] fills in the checkout and the follow-up switch.
    pub(super) invariants: Option<invariants::Settings>,
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
    /// `None` when the observation could not decide the owner; resolved on
    /// demand by `observation::hydrate_owner`.
    owner: Option<String>,
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
    /// Why this pull request is not merging, and since when.
    #[serde(flatten)]
    blocked: blocked_on::Stall,
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
    /// Counts by `blocked_on` token and the oldest blocked pull requests.
    blocked_summary: blocked_on::BlockedSummary,
    cancellations: Vec<CancellationReport>,
    stale_pr_run_wedge: StalePrRunWedgeRepoStatus,
    native_auto_merge_backstop: native_arm::NativeArmRepoStatus,
    errors: Vec<String>,
    /// Members of rollups that landed lately (merge queue included), closed
    /// or left open ([`crate::rollup::reconcile`]). Absent unless the base
    /// takes rollups.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rollup_reconciliation: Vec<String>,
    /// Uncertain merge-queue admissions this pass settled from GitHub, or
    /// left uncertain with the reason ([`mutation_outcome::reconcile`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    uncertain_mutations: Vec<crate::merge_queue_control::UncertainReconciliation>,
    /// What this repository's pass asked GitHub, by call site
    /// ([`crate::gh_pass`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    api_calls: Option<crate::gh_pass::GhPassReport>,
    /// Durable provider cooldown that deferred this repository's observation.
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_limit_deferred: Option<pass_budget::RateLimitCooldown>,
    /// Why this repository made the pass exit 1: each entry is a category,
    /// then the PR numbers or count behind it. Absent when the repository
    /// was healthy, so a non-zero exit with empty `errors` is explained here.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exit_reasons: Vec<String>,
    /// Dwell-time limits, overdue pull requests and the next-to-admit
    /// ranking ([`dwell_pass`]). Absent when the repository was not observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    dwell: Option<dwell_pass::DwellRepoReport>,
    /// The merge groups' verdict on the base, and what the hold and the fix
    /// fast-track did this pass ([`base_poison`]). Absent when the base does
    /// not ask for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    base_poison: Option<Value>,
    /// Required checks re-run (or, in shadow, that would be) because they
    /// failed for an infrastructure reason, and outage findings
    /// ([`infra_rerun`]). Absent when the repository was not observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    infra_rerun: Option<infra_rerun::RepoStatus>,
    /// The same required check failing the same test on many open pull
    /// requests: base-level episodes ([`red_everywhere`]). Absent when the
    /// repository was not observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    red_everywhere: Option<red_everywhere::RepoStatus>,
    /// The tick-start invariants ([`invariants`]): `{id, ok, detail, fix}`
    /// per check. Absent when they are off or the repository was not
    /// observed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    invariants: Vec<invariants::Invariant>,
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
    #[serde(default)]
    wake_records: BTreeMap<String, String>,
    /// Consecutive passes on which a queued head's approval, live state or
    /// policy was unreadable, keyed `repo#pr:head`. See `unapproved_admission`.
    #[serde(default)]
    dequeue_unreadable: BTreeMap<String, u32>,
    /// Superseded by `steward_removals`; read so an older ledger still
    /// loads, and pruned.
    #[serde(default)]
    steward_dequeues: BTreeMap<String, u32>,
    /// When the steward itself removed each head from the queue, keyed
    /// `repo#pr:head` (RFC 3339); exactly those removals do not count toward
    /// the head's ejection cap.
    #[serde(default)]
    steward_removals: BTreeMap<String, Vec<String>>,
    /// Pull requests the last pass deferred (`pass_budget`), keyed by
    /// lowercase repository; the next pass evaluates them first.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pass_budget_deferred: BTreeMap<String, Vec<u64>>,
    /// Repository observations deferred after a provider rate-limit refusal,
    /// keyed by credential-scope hash plus lowercase repository.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    rate_limit_cooldowns: BTreeMap<String, pass_budget::RateLimitCooldown>,
    /// How long each open pull request has been in its blocked state, the
    /// acts taken and the last ranking, keyed by lowercase repository. See
    /// `dwell_pass`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    dwell: BTreeMap<String, crate::merge_steward::dwell::DwellRepoState>,
    /// The last superseded check of each open head, keyed `repo#pr`
    /// (lowercase repository). See `superseded`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    superseded_checks: BTreeMap<String, superseded::Check>,
    /// This pass only: the rate-limit error that stopped the superseded
    /// check, reported in the repository's `errors` so the pass's cooldown
    /// sees it. Never persisted.
    #[serde(skip)]
    superseded_rate_limit: Option<String>,
    /// Queue entries removed while the base was poisoned, in queue order,
    /// keyed `repo@base`; restored first once the poison clears. See
    /// `poison_pause`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    poison_paused: BTreeMap<String, Vec<poison_pause::PausedEntry>>,
    /// Infra re-runs per check on an exact head, keyed
    /// `repo#pr@head:check`. See `infra_rerun`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    infra_reruns: BTreeMap<String, infra_rerun::Record>,
    /// When each outage finding was first reported and last seen, keyed
    /// `repo|check|step`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    infra_outages: BTreeMap<String, infra_rerun::OutageSeen>,
    /// Red-everywhere episodes keyed `repo|check|test`: first sighting and
    /// member heads. See `red_everywhere`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    red_everywhere: BTreeMap<String, red_everywhere::Episode>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_comment_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    performed_via_github_app_id: Option<u64>,
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

#[derive(Clone, Deserialize, Serialize)]
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
    /// Every exact-head approval verdict this pass read, by PR number: the
    /// head it judged and, when refused, why. The report's `blocked_on` reads
    /// it instead of asking GitHub again.
    approval_blockers: std::cell::RefCell<ApprovalBlockers>,
    /// Open pull requests' commits and approvals this pass read for the
    /// carried-commit admission guard. See `carried_commits`.
    carried_commits: std::cell::RefCell<carried_commits::PassCache>,
    /// Changed files this pass read for the generated-path batch guard. See
    /// `generated_paths`.
    changed_files: std::cell::RefCell<generated_paths::PassCache>,
    /// The base's ejection guards ([`crate::merge_steward::EjectionGuards`]).
    ejection_guards: crate::merge_steward::EjectionGuards,
    /// This pass's merge-group reading, when the base asks for one. See
    /// `base_poison`.
    base_poison: Option<&'a base_poison::RepoPoison>,
}

/// Exact-head approval gate reads made during one pass, by PR number.
type ApprovalBlockers = BTreeMap<u64, blocked_on::GateRecord>;

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
pub(crate) use handoff::{
    StewardHandoffArgs, steward_handoff_command, steward_handoff_transfer_report,
};
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
) -> Result<Option<crate::file_lock::LockedFile>, String> {
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
        Ok(()) => Ok(Some(crate::file_lock::LockedFile::new(file))),
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

/// Error text when the repository's admission authority moved while its runs
/// were being inspected.
const ADMISSION_AUTHORITY_CHANGED: &str =
    "admission authority changed during active-run inspection";

/// Attempts at a consistent admission plan before the authority change is
/// reported as an error.
const ADMISSION_AUTHORITY_ATTEMPTS: usize = 3;

/// Run `plan` until it completes under an admission authority that held still.
///
/// The authority is every open pull request's head and managed state plus the
/// merge queue's group heads, so an unrelated push or queue movement anywhere
/// in the repository during the seconds an inspection takes invalidates it.
/// On a busy repository that is routine: every observed failure on the fleet
/// coincided with a pull-request push or merge-group run in the preceding
/// minutes. A plan made under an authority that moved is discarded and made
/// again from fresh reads; only a fence that never holds is an error. Any
/// other error returns at once.
fn with_stable_admission_authority<T>(
    mut plan: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let mut last = String::new();
    for _ in 0..ADMISSION_AUTHORITY_ATTEMPTS {
        match plan() {
            Err(error) if error == ADMISSION_AUTHORITY_CHANGED => last = error,
            outcome => return outcome,
        }
    }
    Err(format!(
        "{last} ({ADMISSION_AUTHORITY_ATTEMPTS} consecutive attempts)"
    ))
}

fn observe_admission_candidates(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    labels: &[String],
    require_authority_fence: bool,
) -> Result<(RepoObservation, Vec<RunCancellation>, Vec<u64>), String> {
    with_stable_admission_authority(|| {
        let observation = observe_repo(actions, repo, base, false)?;
        let (candidates, blocking_only) = admission_candidates(actions, &observation, labels)?;
        // A read-only verdict has no subsequent mutation to protect. Avoid a
        // second full GitHub census there; the apply path keeps the exact-head
        // fence before any destructive action.
        if require_authority_fence {
            fence_admission_authority(actions, &observation)?;
        }
        Ok((observation, candidates, blocking_only))
    })
}

fn refreshed_admission_authority(
    actions: &GitHubActions,
    observation: &RepoObservation,
) -> Result<(Vec<ObservedPr>, BTreeMap<u64, String>), String> {
    let (_, positions, heads, _) =
        merge_queue_snapshot(actions, &observation.repo, &observation.base)?;
    let mut prs = pull_requests(actions, &observation.repo, &observation.base, &positions)?;
    hydrate_census_check_identities(actions, &observation.repo, &[], &mut prs)?;
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
        return Err(ADMISSION_AUTHORITY_CHANGED.to_owned());
    }
    Ok(())
}

fn revalidate_active_admission_candidates(
    actions: &GitHubActions,
    observation: &RepoObservation,
    labels: &[String],
) -> Result<(Vec<RunCancellation>, Vec<u64>), String> {
    with_stable_admission_authority(|| {
        let mut refreshed = observation.clone();
        let (prs, heads) = refreshed_admission_authority(actions, observation)?;
        refreshed.prs = prs;
        refreshed.merge_group_heads = heads;
        refreshed.runs = active_runs(actions, &observation.repo)?;
        let candidates = admission_candidates(actions, &refreshed, labels)?;
        fence_admission_authority(actions, &refreshed)?;
        Ok(candidates)
    })
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

#[cfg(test)]
pub(super) fn admission_clean_command<W: Write>(
    args: &AdmissionCleanArgs,
    cwd: &Path,
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    admission_clean_command_with_reads(
        args,
        cwd,
        mode,
        runtime_paths,
        actions,
        actions,
        json_output,
        stdout,
    )
}

/// `runner admission-clean`. `read_actions` makes every census, including
/// the ones around a cancellation, so they spend a dedicated admission budget
/// and reuse recent per-head reads; `actions` performs the cancellation and
/// its recovery.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn admission_clean_command_with_reads<W: Write>(
    args: &AdmissionCleanArgs,
    cwd: &Path,
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    read_actions: &GitHubActions,
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
    let deadline = Instant::now() + ADMISSION_OBSERVATION_TIMEOUT;
    let admission_actions = actions.clone().with_absolute_deadline(deadline);
    let actions = &admission_actions;
    let admission_read_actions = read_actions.clone().with_absolute_deadline(deadline);
    let read_actions = &admission_read_actions;
    let (mut observation, mut candidates, mut blocking_only) =
        match observe_admission_candidates(read_actions, &repo, &base, &labels, args.apply) {
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
    // The steward ledger lock serializes ledger *writers*. A steward apply
    // pass holds it for its whole multi-repo pass (minutes), so an admission
    // that only reads must not wait on it or defer because of it: the ledger
    // is published by atomic rename, so `load_ledger` without the lock is a
    // consistent snapshot. Admission takes the lock only on the path where it
    // resumes a pending cancellation or cancels a run itself; there a held
    // lock is a real queue hold and still defers `stewardship_in_progress`.
    // Holding the lock on a read-only path would not protect the JIT mint
    // either, because the lock is released when this command returns.
    //
    // Report-only admission is an observation, not a mutation precondition.
    // Return the snapshot verdict without running a second full census; the
    // apply path below performs the exact-head fence.
    if !args.apply {
        if candidates.is_empty() && blocking_only.is_empty() && observed_pending.is_empty() {
            return_verdict!(Admit, "clean", &canonical_repo, &[], None);
        }
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
    if candidates.is_empty() && blocking_only.is_empty() && observed_pending.is_empty() {
        // Read-only clean path: a fresh lock-free snapshot (see above). A
        // pending cancellation a running steward published since the first
        // read still defers, as `cancellation_pending`.
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
            match revalidate_active_admission_candidates(read_actions, &observation, &labels) {
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

    // A pending cancellation is steward-owned crash-recovery state: resuming
    // it rewrites that record, so it needs the steward ledger lock and still
    // defers while an apply pass holds it. Cancelling a superseded run does
    // not: it writes no pending record, the per-run mutation guard serializes
    // it against a steward cancelling the same run, and its only ledger write
    // is an audit line. While a steward pass holds the lock (minutes), that
    // audit line goes to an admission-owned sidecar instead of waiting, because
    // the steward rewrites its whole in-memory ledger on save and would drop a
    // line written into the shared file underneath it.
    let ledger_lock = if observed_pending.is_empty() {
        match try_acquire_ledger_lock(&ledger_path) {
            Ok(lock) => lock,
            Err(error) => return_verdict!(
                Error,
                "mutation_failed",
                &canonical_repo,
                &blocker_run_ids,
                Some(error.message())
            ),
        }
    } else {
        Some(acquire_admission_ledger_lock!(&blocker_run_ids))
    };
    let audit_path = if ledger_lock.is_some() {
        ledger_path.clone()
    } else {
        admission_audit_path(&ledger_path)
    };
    let (mut ledger, pending) = if ledger_lock.is_some() {
        let ledger = load_ledger(&ledger_path)?;
        let pending = pending_admission_cancellations(&ledger, &canonical_repo, &base);
        (ledger, pending)
    } else {
        // A pending cancellation the running steward published since the
        // first snapshot is still a queue hold the steward will resume.
        let snapshot = load_ledger(&ledger_path)?;
        let late_pending = pending_admission_cancellations(&snapshot, &canonical_repo, &base);
        if !late_pending.is_empty() {
            let blockers = admission_blocker_run_ids(&candidates, &blocking_only, &late_pending);
            return_verdict!(
                Defer,
                "stewardship_in_progress",
                &canonical_repo,
                &blockers,
                Some("another steward apply pass is already running")
            );
        }
        (StewardLedger::default(), Vec::new())
    };
    let persist = |ledger: &StewardLedger| {
        if ledger_lock.is_some() {
            save_ledger(&ledger_path, ledger)
        } else {
            append_admission_audit(&audit_path, ledger)
        }
    };
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
    // The pre-cancel and post-cancel censuses are reads, so they spend the
    // admission credential and reuse its per-head cache. A cached head can
    // only hide a newer handoff status, which skips a cancellation rather than
    // adding one; `apply_run_cancellation` still acts with `[github.auth]`.
    match observe_admission_candidates(read_actions, &canonical_repo, &base, &labels, true) {
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
            &audit_path,
            &mut ledger,
            &mutation_control,
        );
        if let Some(error) = error {
            let _ = persist(&ledger);
            // The merge-queue control lock is taken without blocking, and a
            // steward or another admission lane holds it for one GitHub call.
            // That is a transient queue hold, not an operational failure.
            if merge_queue_lock_contended(&error) {
                return_verdict!(
                    Defer,
                    "mutation_in_progress",
                    &canonical_repo,
                    &blocker_run_ids,
                    Some(&error)
                );
            }
            return_verdict!(
                Error,
                "mutation_failed",
                &canonical_repo,
                &blocker_run_ids,
                Some(&error)
            );
        }
    }
    persist(&ledger)?;
    let (remaining, remaining_blocking) =
        match observe_admission_candidates(read_actions, &canonical_repo, &base, &labels, true) {
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

#[allow(clippy::too_many_lines)]
pub(super) fn steward_command<W: Write>(
    args: &StewardCommandArgs,
    cwd: &Path,
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let (mut normalized_args, repos) = normalized_steward_args(args, cwd)?;
    normalized_args.unapproved_dequeue =
        unapproved_admission::DequeueMode::from_machine_global(&runtime_paths.global_dir)
            .map_err(|error| CliFailure::new(2, error))?;
    if normalized_args.invariants.is_some() {
        normalized_args.invariants = Some(
            invariants::Settings::from_machine_global(
                &runtime_paths.global_dir,
                Some(cwd.to_path_buf()),
            )
            .map_err(|error| CliFailure::new(2, error))?,
        );
    }
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
    let mut rate_limited: Option<String> = None;
    for repo in repos {
        let scope_key = actions.rate_limit_scope_key(&repo);
        let cooldown_key = pass_budget::rate_limit_cooldown_key(&scope_key, &repo);
        let now_epoch = Utc::now().timestamp();
        if let Some(cooldown) = pass_budget::active_rate_limit_cooldown(
            &ledger.rate_limit_cooldowns,
            &cooldown_key,
            now_epoch,
        ) {
            unhealthy = true;
            reports.push(deferred_rate_limit_repo_report(
                &repo,
                &args.base,
                cooldown.clone(),
                &ledger,
            ));
            continue;
        }
        // Expiry is itself a state transition. Remove the stale receipt so a
        // successful probe can clear it from the durable ledger.
        ledger.rate_limit_cooldowns.remove(&cooldown_key);
        // A rate limit spends the token for every repository alike: the rest
        // are reported unread rather than spending the pass on errors.
        if let Some(error) = rate_limited.as_ref() {
            let cooldown = record_rate_limit_cooldown(
                &repo,
                &scope_key,
                error,
                &mut ledger,
                args.apply.then_some(&ledger_path),
            );
            if let Ok(cooldown) = cooldown {
                unhealthy = true;
                reports.push(deferred_rate_limit_repo_report(
                    &repo, &args.base, cooldown, &ledger,
                ));
            } else {
                unhealthy = true;
                reports.push(unreadable_repo_report(
                    &repo,
                    &args.base,
                    &ledger,
                    format!("{}: not observed this pass; {error}", pass_budget::DEFERRED),
                ));
            }
            continue;
        }
        let (report, failed) = steward_repo(
            actions,
            args,
            &repo,
            &ledger_path,
            &mut ledger,
            &mut remaining_preemptions,
            mutation_control.as_ref(),
            &mut recovery_errors,
            &mut recovery_cancellations,
        );
        if report.rate_limit_deferred.is_some()
            || report
                .errors
                .iter()
                .any(|error| pass_budget::is_rate_limited(error))
        {
            rate_limited = Some(format!("GitHub rate-limited observing {repo}"));
        }
        unhealthy |= failed;
        reports.push(report);
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

/// `actions` for one repository's pass: scoped to `repo` for the token
/// helper (a scheduled pass runs outside any checkout, and one checkout's
/// remote must never mint the token for another repository), counting its
/// GitHub reads and reusing the base-policy and content-addressed answers it
/// may cache ([`crate::gh_pass`]).
fn repo_pass_actions(
    actions: &GitHubActions,
    repo: &str,
) -> (GitHubActions, std::sync::Arc<crate::gh_pass::GhPass>) {
    let pass = std::sync::Arc::new(crate::gh_pass::GhPass::caching());
    let scoped = actions
        .clone()
        .with_repo_override(repo)
        .with_pass(std::sync::Arc::clone(&pass));
    (scoped, pass)
}

/// Close the members of rollups that landed on `base` lately, however they
/// landed ([`crate::rollup::reconcile_if_enabled`]); nothing on a base that
/// does not take rollups.
fn reconcile_rollups(actions: &GitHubActions, repo: &str, base: &str, report: &mut RepoReport) {
    let run_gh = |gh_args: &[String]| actions.run_gh(gh_args).map_err(|error| error.to_string());
    match crate::rollup::reconcile_if_enabled(&run_gh, repo, base, chrono::Utc::now()) {
        Ok(lines) => report.rollup_reconciliation = lines,
        Err(error) => report
            .errors
            .push(format!("rollup reconciliation: {error}")),
    }
}

/// One repository of a steward pass: observe it, apply its plan, settle its
/// rollups. Its own time budget starts here, so an earlier repository never
/// spends a later one's. Returns the report and whether the repository is
/// unhealthy.
#[allow(clippy::too_many_arguments)]
fn steward_repo(
    actions: &GitHubActions,
    pass_args: &StewardCommandArgs,
    repo: &str,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    remaining_preemptions: &mut usize,
    mutation_control: Option<&MutationControl>,
    recovery_errors: &mut BTreeMap<String, Vec<String>>,
    recovery_cancellations: &mut BTreeMap<String, Vec<CancellationReport>>,
) -> (RepoReport, bool) {
    // Scope the token helper to this exact repository: a scheduled pass
    // runs from a directory that is not a checkout, and one checkout's
    // remote must never mint the token for another repository.
    let (scoped_actions, pass) = repo_pass_actions(actions, repo);
    let actions = &scoped_actions;
    let (mut report, unhealthy) = steward_repo_observed(
        actions,
        pass_args,
        repo,
        ledger_path,
        ledger,
        remaining_preemptions,
        mutation_control,
        recovery_errors,
        recovery_cancellations,
    );
    report.api_calls = Some(pass.report());
    if let (Some(api_calls), Some(sizer)) = (
        report.api_calls.as_mut(),
        actions
            .inproc_reads()
            .and_then(crate::github_read::InprocReads::batch),
    ) {
        // A dry run reports the next size but leaves the live steward's state.
        let next = if pass_args.apply {
            sizer.save().unwrap_or_else(|error| {
                api_calls.fallbacks.push(crate::gh_pass::FallbackReport {
                    reason: "rollup batch size not saved".to_owned(),
                    count: 1,
                    last_detail: error,
                });
                sizer.next_size()
            })
        } else {
            sizer.next_size()
        };
        api_calls.rollup_batch = Some(crate::gh_pass::RollupBatchReport {
            heads_per_query: sizer.size(),
            next_heads_per_query: next,
        });
    }
    if pass_args.apply
        && let (Some(control), Some(dwell)) = (mutation_control, report.dwell.as_mut())
        && let Err(error) = dwell_pass::write_snapshot(
            &control.state_dir,
            &report.repo,
            &report.base,
            dwell,
            report.api_calls.as_ref(),
            Utc::now(),
        )
    {
        dwell.errors.push(error);
    }
    if unhealthy {
        report.exit_reasons = exit_reasons(&report);
    }
    (report, unhealthy)
}

/// The categories that make a repository's pass exit 1, derived from its
/// report. Pull-request refusals (an unapproved carried commit, a failed
/// mutation) land in `prs[].error` and leave `errors` empty, so they need
/// their own entry; `unclassified` marks an unhealthy repository none of the
/// report fields explain.
fn exit_reasons(report: &RepoReport) -> Vec<String> {
    let mut reasons = Vec::new();
    if !report.errors.is_empty() {
        reasons.push(format!("repo_errors:{}", report.errors.len()));
    }
    let numbers = |numbers: Vec<String>| numbers.join(",");
    let pr_errors = report
        .prs
        .iter()
        .filter(|pr| pr.error.is_some())
        .map(|pr| format!("#{}", pr.number))
        .collect::<Vec<_>>();
    if !pr_errors.is_empty() {
        reasons.push(format!("pr_errors:{}", numbers(pr_errors)));
    }
    let failed_runs = report
        .cancellations
        .iter()
        .filter(|cancellation| cancellation.error.is_some())
        .map(|cancellation| cancellation.run_id.to_string())
        .collect::<Vec<_>>();
    if !failed_runs.is_empty() {
        reasons.push(format!("cancellation_errors:{}", numbers(failed_runs)));
    }
    if report.rate_limit_deferred.is_some() {
        reasons.push("rate_limited".to_owned());
    }
    if report
        .uncertain_mutations
        .iter()
        .any(|result| result.outcome == "uncertain")
    {
        reasons.push("uncertain_mutations".to_owned());
    }
    if reasons.is_empty() {
        reasons.push("unclassified".to_owned());
    }
    reasons
}

/// [`steward_repo`] once `actions` is scoped to `repo` and counting its
/// GitHub reads.
#[allow(clippy::too_many_arguments)]
fn steward_repo_observed(
    actions: &GitHubActions,
    pass_args: &StewardCommandArgs,
    repo: &str,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    remaining_preemptions: &mut usize,
    mutation_control: Option<&MutationControl>,
    recovery_errors: &mut BTreeMap<String, Vec<String>>,
    recovery_cancellations: &mut BTreeMap<String, Vec<CancellationReport>>,
) -> (RepoReport, bool) {
    let mut repo_args = pass_args.clone();
    if let Some(budget) = pass_args.pass_budget {
        repo_args.pass_deadline = Some(Instant::now() + budget);
    }
    let args = &repo_args;
    match observe_repo_with_status_history(
        actions,
        repo,
        &args.base,
        args.preempt_capacity,
        Some(&status_history_path(ledger_path, repo)),
    ) {
        Ok(observation) => {
            let scope_key = actions.rate_limit_scope_key(repo);
            let cooldown_key = pass_budget::rate_limit_cooldown_key(&scope_key, repo);
            // A successful repository-wide probe proves this credential/repo
            // scope is usable again; clear its stale receipt before planning.
            ledger.rate_limit_cooldowns.remove(&cooldown_key);
            let (mut report, failed, planned_preemptions) = apply_repo_plan(
                actions,
                args,
                &observation,
                ledger_path,
                ledger,
                *remaining_preemptions,
                mutation_control,
            );
            if let Some(errors) = recovery_errors.remove(&observation.repo) {
                report.errors.extend(errors);
            }
            if let Some(cancellations) = recovery_cancellations.remove(&observation.repo) {
                report.cancellations.extend(cancellations);
            }
            if args.apply && pass_budget::deadline_passed(args.pass_deadline) {
                report
                    .rollup_reconciliation
                    .push("deferred: the pass time budget is spent".to_owned());
            } else if args.apply {
                reconcile_rollups(actions, &observation.repo, &args.base, &mut report);
            }
            *remaining_preemptions = remaining_preemptions.saturating_sub(planned_preemptions);
            if let Some(settings) = args.invariants.as_ref() {
                report.invariants = invariants::run(
                    actions,
                    settings,
                    &observation,
                    &report.prs,
                    &report.merge_path,
                    &args.approval_context,
                    &invariants::state_path(ledger_path),
                    args.apply,
                    Utc::now(),
                );
            }
            let unhealthy = failed || !report.errors.is_empty();
            (report, unhealthy)
        }
        Err(error) if pass_budget::is_rate_limited(&error) => {
            let scope_key = actions.rate_limit_scope_key(repo);
            let cooldown = record_rate_limit_cooldown(
                repo,
                &scope_key,
                &error,
                ledger,
                args.apply.then_some(ledger_path),
            );
            match cooldown {
                Ok(cooldown) => (
                    deferred_rate_limit_repo_report(repo, &args.base, cooldown, ledger),
                    true,
                ),
                Err(persist_error) => {
                    let mut report = unreadable_repo_report(repo, &args.base, ledger, error);
                    report.errors.push(persist_error);
                    (report, true)
                }
            }
        }
        Err(error) => (
            unreadable_repo_report(repo, &args.base, ledger, error),
            true,
        ),
    }
}

/// Where the census keeps the commit-status histories it may reuse on the
/// next pass of `repo`: beside the ledger, one file per repository, since
/// several repositories' passes share one ledger and each rewrites its file.
fn status_history_path(ledger_path: &Path, repo: &str) -> PathBuf {
    let slug = repo
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '~'
            }
        })
        .collect::<String>();
    ledger_path.with_extension(format!("status-history.{slug}.json"))
}

fn record_rate_limit_cooldown(
    repo: &str,
    scope_key: &str,
    error: &str,
    ledger: &mut StewardLedger,
    ledger_path: Option<&Path>,
) -> Result<pass_budget::RateLimitCooldown, String> {
    let observed_at_epoch = Utc::now().timestamp();
    let cooldown = pass_budget::RateLimitCooldown {
        repo: repo.to_owned(),
        scope_key: scope_key.to_owned(),
        until_epoch: pass_budget::rate_limit_cooldown_until(error, observed_at_epoch),
        observed_at_epoch,
        detail: error.to_owned(),
    };
    let key = pass_budget::rate_limit_cooldown_key(scope_key, repo);
    ledger.rate_limit_cooldowns.insert(key, cooldown.clone());
    if let Some(path) = ledger_path {
        save_ledger(path, ledger).map_err(|error| {
            format!(
                "could not persist rate-limit cooldown receipt: {}",
                error.message()
            )
        })?;
    }
    Ok(cooldown)
}

fn deferred_rate_limit_repo_report(
    repo: &str,
    base: &str,
    cooldown: pass_budget::RateLimitCooldown,
    ledger: &StewardLedger,
) -> RepoReport {
    RepoReport {
        base_poison: None,
        rollup_reconciliation: Vec::new(),
        uncertain_mutations: Vec::new(),
        repo: repo.to_owned(),
        base: base.to_owned(),
        allow_auto_merge: false,
        merge_queue: false,
        merge_path: "deferred_rate_limit".to_owned(),
        required_contexts: Vec::new(),
        prs: Vec::new(),
        blocked_summary: blocked_on::BlockedSummary::default(),
        cancellations: Vec::new(),
        stale_pr_run_wedge: stale_pr_wedge::repo_status(None, Vec::new(), ledger, repo),
        native_auto_merge_backstop: native_arm::NativeArmRepoStatus::default(),
        errors: Vec::new(),
        api_calls: None,
        rate_limit_deferred: Some(cooldown),
        exit_reasons: Vec::new(),
        dwell: None,
        infra_rerun: None,
        red_everywhere: None,
        invariants: Vec::new(),
    }
}

fn unreadable_repo_report(
    repo: &str,
    base: &str,
    ledger: &StewardLedger,
    error: String,
) -> RepoReport {
    RepoReport {
        base_poison: None,
        rollup_reconciliation: Vec::new(),
        uncertain_mutations: Vec::new(),
        api_calls: None,
        repo: repo.to_owned(),
        base: base.to_owned(),
        allow_auto_merge: false,
        merge_queue: false,
        merge_path: "unreadable".to_owned(),
        required_contexts: Vec::new(),
        prs: Vec::new(),
        blocked_summary: blocked_on::BlockedSummary::default(),
        cancellations: Vec::new(),
        stale_pr_run_wedge: stale_pr_wedge::repo_status(None, Vec::new(), ledger, repo),
        native_auto_merge_backstop: native_arm::NativeArmRepoStatus::default(),
        errors: vec![error],
        rate_limit_deferred: None,
        exit_reasons: Vec::new(),
        dwell: None,
        infra_rerun: None,
        red_everywhere: None,
        invariants: Vec::new(),
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
            base_poison: None,
            rollup_reconciliation: Vec::new(),
            uncertain_mutations: Vec::new(),
            api_calls: None,
            repo: repo.clone(),
            base,
            allow_auto_merge: false,
            merge_queue: false,
            merge_path: "pending_cancellation_recovery".to_owned(),
            required_contexts: Vec::new(),
            prs: Vec::new(),
            blocked_summary: blocked_on::BlockedSummary::default(),
            cancellations,
            stale_pr_run_wedge: stale_pr_wedge::repo_status(None, Vec::new(), ledger, &repo),
            native_auto_merge_backstop: native_arm::NativeArmRepoStatus::default(),
            errors,
            rate_limit_deferred: None,
            exit_reasons: Vec::new(),
            dwell: None,
            infra_rerun: None,
            red_everywhere: None,
            invariants: Vec::new(),
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
            base_poison: None,
            rollup_reconciliation: Vec::new(),
            uncertain_mutations: Vec::new(),
            api_calls: None,
            repo: "steward".to_owned(),
            base: base.to_owned(),
            allow_auto_merge: false,
            merge_queue: false,
            merge_path: "unreadable".to_owned(),
            required_contexts: Vec::new(),
            prs: Vec::new(),
            blocked_summary: blocked_on::BlockedSummary::default(),
            cancellations: Vec::new(),
            stale_pr_run_wedge: stale_pr_wedge::repo_status(None, Vec::new(), ledger, "steward"),
            native_auto_merge_backstop: native_arm::NativeArmRepoStatus::default(),
            errors: vec![message],
            rate_limit_deferred: None,
            exit_reasons: Vec::new(),
            dwell: None,
            infra_rerun: None,
            red_everywhere: None,
            invariants: Vec::new(),
        });
    }
}

/// Whether `error` is another Shipyard process holding the merge-queue
/// mutation control lock or authority, which the guard takes without waiting.
fn merge_queue_lock_contended(error: &str) -> bool {
    error.contains("another Shipyard process is performing a merge-queue mutation")
        || error.contains("another Shipyard process owns merge-queue mutation authority")
}

/// The admission-owned audit file beside the steward ledger. Admission writes
/// here only while a steward apply pass holds the steward ledger lock.
fn admission_audit_path(ledger_path: &Path) -> PathBuf {
    ledger_path.with_file_name("merge-steward-admission-audit.json")
}

/// How long admission waits for another admission to finish appending to the
/// audit sidecar. Each holder only reads, appends and renames one small file.
const ADMISSION_AUDIT_LOCK_WAIT: Duration = Duration::from_secs(5);

/// Append `ledger`'s audit lines to the admission audit sidecar under its own
/// lock, which no steward pass ever holds.
fn append_admission_audit(path: &Path, ledger: &StewardLedger) -> Result<(), CliFailure> {
    if ledger.audit.is_empty() {
        return Ok(());
    }
    let deadline = Instant::now() + ADMISSION_AUDIT_LOCK_WAIT;
    let _lock = loop {
        if let Some(lock) = try_acquire_ledger_lock(path)? {
            break lock;
        }
        if Instant::now() >= deadline {
            return Err(CliFailure::new(
                1,
                format!("could not lock admission audit {}", path.display()),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut sidecar = load_ledger(path)?;
    sidecar.audit.extend(ledger.audit.iter().cloned());
    if sidecar.audit.len() > 1_000 {
        sidecar.audit.drain(..sidecar.audit.len() - 1_000);
    }
    save_ledger(path, &sidecar)
}

fn try_acquire_ledger_lock(
    path: &Path,
) -> Result<Option<crate::file_lock::LockedFile>, CliFailure> {
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
        Ok(()) => Ok(Some(crate::file_lock::LockedFile::new(file))),
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

fn acquire_ledger_lock(path: &Path) -> Result<crate::file_lock::LockedFile, CliFailure> {
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

mod base_poison;

/// The required checks of `base`, for a caller outside the steward that
/// judges merge groups ([`crate::base_health::merge_group`]).
pub(super) fn required_contexts(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Vec<RequiredCheck>, String> {
    observation::required_checks(actions, repo, base)
}

/// The one fix for the poison `health` names ([`base_poison::named_fixes`]
/// narrowed by [`base_poison::choose_fix`]), the same pick the steward makes.
pub(super) fn named_fixes(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    repo: &str,
    base: &str,
    health: &crate::base_health::merge_group::MergeGroupHealth,
) -> (BTreeMap<u64, Vec<String>>, Option<String>) {
    let (fixes, error) =
        base_poison::named_fixes(run_gh, repo, base, &health.poisoned_contexts(), Utc::now());
    (base_poison::choose_fix(fixes, health), error)
}
mod blocked_on;
mod cancellation;
mod cancellation_recovery;
mod cancellation_revalidation;
mod cancellation_terminalization;
mod capacity_cancellation;
mod carried_commits;
mod code_owner_wait;
pub(super) mod dwell_pass;
mod exact_head_merge;
mod generated_paths;
mod head_rollup;
pub(super) mod infra_rerun;
pub(super) mod invariants;
mod ledger;
mod mutation_outcome;
mod native_arm;
mod next_action;
mod observation;
mod pass_budget;
mod poison_pause;
mod pr_mutations;
pub(super) mod queue_drain;
mod queue_priority_recovery;
pub(super) mod read_probe;
mod red_everywhere;
mod render;
mod resume_record;
mod shadow_ledger;
mod superseded;
mod terminal_handoff;
pub(super) mod train;
mod unapproved_admission;
pub(super) use carried_commits::STACKED_LABEL_PREFIX;
pub(super) use infra_rerun::Mode as InfraRerunMode;
pub(super) use unapproved_admission::DequeueMode;

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
use generated_paths::read_ejection_guards;
use ledger::{attempt_key, check_attempt_key, load_ledger, record_audit, save_ledger};
/// How `repo`'s `base` admits an approved head, for `shipyard pr`'s next-steps
/// block. Read-only.
pub(super) fn observe_next_steps(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> crate::pr_next_steps::NextSteps {
    match observation::admission_policy(actions, repo, base) {
        Ok(policy) => crate::pr_next_steps::NextSteps::from_policy(&policy),
        Err(error) => crate::pr_next_steps::NextSteps::unavailable(error),
    }
}

/// One pull request's live answer for `shipyard wait pr <n> --state
/// actionable`: the `blocked_on` row a dry-run admission pass gives that pull
/// request alone, with its next action, as one JSON object. A merged or
/// closed pull request, one based on a branch other than the repository
/// default, and one whose diff against its base is empty are answered before
/// any admission read. Read-only: the pass is a dry run and the steward
/// ledger is only read.
pub(super) fn pr_next_action_snapshot(
    actions: &GitHubActions,
    state_dir: &Path,
    repo: &str,
    number: u64,
) -> Result<Value, String> {
    let row = gh_json(
        actions,
        &[
            "pr".to_owned(),
            "view".to_owned(),
            number.to_string(),
            "--repo".to_owned(),
            repo.to_owned(),
            "--json".to_owned(),
            observation::PR_VIEW_FIELDS.to_owned(),
        ],
        "pull request",
    )?;
    let settings = gh_json(
        actions,
        &["api".to_owned(), format!("repos/{repo}")],
        "repository settings",
    )?;
    let repo = observation::canonical_repo_name(&settings)?;
    let default_branch = settings
        .get("default_branch")
        .and_then(Value::as_str)
        .filter(|branch| !branch.is_empty())
        .unwrap_or("main")
        .to_owned();
    let text = |key: &str| {
        row.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let state = text("state").to_ascii_uppercase();
    let base = text("baseRefName");
    let mut snapshot = json!({
        "repo": repo,
        "number": number,
        "state": state,
        "merged": state == "MERGED",
        "headRefOid": text("headRefOid"),
        "headRefName": text("headRefName"),
        "baseRefName": base,
        "default_branch": default_branch,
    });
    let pre_admission = match state.as_str() {
        "MERGED" => Some((
            "merged".to_owned(),
            next_action::NextAction::merged(
                &text("headRefName"),
                &base,
                row.get("mergedAt").and_then(Value::as_str),
            ),
        )),
        "CLOSED" => Some(("closed".to_owned(), next_action::NextAction::closed())),
        _ if !base.eq_ignore_ascii_case(&default_branch) => {
            let parent = open_pull_request_for_head(actions, &repo, &base);
            Some((
                format!("stacked_on:{base}"),
                next_action::NextAction::stacked(&base, &default_branch, parent),
            ))
        }
        _ if row.get("changedFiles").and_then(Value::as_u64) == Some(0) => Some((
            "empty_diff".to_owned(),
            next_action::NextAction::empty_diff(&base),
        )),
        _ => None,
    };
    if let Some((blocked_on, next)) = pre_admission {
        snapshot["blocked_on"] = Value::from(blocked_on);
        snapshot["actionable"] = Value::from(next.actionable());
        snapshot["next_action"] = serde_json::to_value(next).unwrap_or(Value::Null);
        return Ok(snapshot);
    }
    let observation = observation::observe_pull_request(actions, &settings, &base, &row)?;
    let (pr, merge_state, required_contexts, errors) =
        single_pr_dry_run(actions, state_dir, &observation, number)?;
    let actionable = pr
        .blocked
        .next_action
        .as_ref()
        .is_some_and(next_action::NextAction::actionable);
    let value = serde_json::to_value(&pr).map_err(|error| error.to_string())?;
    if let (Some(target), Some(fields)) = (snapshot.as_object_mut(), value.as_object()) {
        for (key, field) in fields {
            target.insert(key.clone(), field.clone());
        }
        target.insert("actionable".to_owned(), Value::from(actionable));
        target.insert("merge_state".to_owned(), Value::from(merge_state));
        target.insert(
            "required_contexts".to_owned(),
            Value::from(required_contexts),
        );
        target.insert("pass_errors".to_owned(), Value::from(errors));
    }
    Ok(snapshot)
}

/// The dry-run admission pass over a one-row observation: the row for
/// `number`, its merge state, the base's required contexts, and the pass's
/// errors. The same `apply_repo_plan` a scheduled pass runs, with every
/// mutation, coalescing, preemption and backstop plan off.
fn single_pr_dry_run(
    actions: &GitHubActions,
    state_dir: &Path,
    observation: &RepoObservation,
    number: u64,
) -> Result<(PrReport, String, Vec<String>, Vec<String>), String> {
    let ledger_path = state_dir.join("merge-steward.json");
    let mut ledger = load_ledger(&ledger_path).map_err(|error| error.message().to_owned())?;
    let args = StewardCommandArgs {
        repos: vec![observation.repo.clone()],
        base: observation.base.clone(),
        opt_out_label: "shipyard:no-auto-merge".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: false,
        coalesce: false,
        preempt_capacity: false,
        max_preemptions_per_head: 1,
        arm_unqueued: false,
        approval_context: crate::merge_steward::DEFAULT_APPROVAL_CONTEXT.to_owned(),
        wake_owner: false,
        unapproved_dequeue: DequeueMode::Report,
        infra_rerun: InfraRerunMode::Off,
        pass_budget: None,
        pass_deadline: None,
        invariants: None,
        apply: false,
        ledger: None,
    };
    let (report, _, _) = apply_repo_plan(
        actions,
        &args,
        observation,
        &ledger_path,
        &mut ledger,
        0,
        None,
    );
    let pr = report
        .prs
        .into_iter()
        .find(|pr| pr.number == number)
        .ok_or_else(|| format!("the admission pass produced no row for #{number}"))?;
    let merge_state = observation
        .prs
        .first()
        .map(|observed| observed.fact.merge_state.clone())
        .unwrap_or_default();
    Ok((pr, merge_state, report.required_contexts, report.errors))
}

/// The open pull request whose head is `branch`, when exactly one exists.
fn open_pull_request_for_head(actions: &GitHubActions, repo: &str, branch: &str) -> Option<u64> {
    gh_json(
        actions,
        &[
            "pr".to_owned(),
            "list".to_owned(),
            "--repo".to_owned(),
            repo.to_owned(),
            "--head".to_owned(),
            branch.to_owned(),
            "--state".to_owned(),
            "open".to_owned(),
            "--json".to_owned(),
            "number".to_owned(),
            "--limit".to_owned(),
            "2".to_owned(),
        ],
        "parent pull request",
    )
    .ok()?
    .as_array()
    .filter(|rows| rows.len() == 1)
    .and_then(|rows| rows[0].get("number").and_then(Value::as_u64))
}

use observation::{
    active_runs, complete_checks_for_head, fetch_run_jobs, fetch_run_jobs_before, gh_json,
    gh_json_before, gh_json_timeout, hydrate_census_check_identities,
    hydrate_required_check_identities, hydrate_required_check_identities_before,
    merge_queue_snapshot, merge_queue_snapshot_before, observe_repo,
    observe_repo_with_status_history, parse_job, parse_pr, parse_run, pull_requests,
    required_checks, resolve_repos,
};
#[cfg(test)]
use pr_mutations::mutate_pr;
use render::{
    enqueue_requirements_pending, is_admin_protection_denied, is_private_free_entitlement,
    render_report,
};

#[cfg(test)]
mod tests;
