//! Opt-in, crash-safe recovery for an in-flight ship whose exact durable work
//! item has fallen out of the queue.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::LoadedConfig;
use crate::evidence::canonical_repository;
use crate::execution_supervisor::WorkerReceipt;
use crate::gh::GhClient;
use crate::identity::RuntimeMode;
use crate::job::{Job, JobKind, JobStatus};
use crate::queue::{Queue, RecoveryEnqueue};
use crate::queue_request::{
    JobResourcePlan, QueueOutcomeStore, QueueRequestError, QueueRequestStore,
    QueuedExecutionEnvelope, QueuedExecutionKind, QueuedExecutionRequest,
};
use crate::ship_liveness::{queue_absent_recovery_enabled, queue_absent_repo_path};
use crate::ship_state::{ShipState, ShipStateStore};
use crate::watch::ship_terminal_verdict;

const RECOVERY_SCHEMA_VERSION: u32 = 1;

/// Request envelopes that remain authoritative for active ship recovery.
///
/// The generic absent-envelope sweep runs much earlier than queue-absence
/// recovery becomes eligible. Preserve exact request provenance for every
/// nonterminal ship state so cleanup cannot destroy the only recovery
/// authority before the supervisor claims it.
pub(crate) fn protected_request_job_ids(
    state_dir: &Path,
    request_store: &QueueRequestStore,
) -> Result<BTreeSet<String>, QueueRequestError> {
    let active_states = ShipStateStore::new(state_dir.join("ship"))?
        .list_active()
        .into_iter()
        .filter(|state| ship_terminal_verdict(state).is_none())
        .collect::<Vec<_>>();
    let envelopes = request_store.list()?;
    Ok(envelopes
        .into_iter()
        .filter(|envelope| {
            active_states
                .iter()
                .any(|state| envelope_matches_state(envelope, state))
        })
        .map(|envelope| envelope.job_id)
        .collect())
}

/// Typed durable recovery disposition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueAbsentRecoveryStatus {
    /// Durable generation chosen; queue insertion may not yet have committed.
    Claimed,
    /// The exact replacement is durably present or was observed present.
    Enqueued,
    /// Automatic recovery stopped and requires a code-capable agent/operator.
    NeedsAgent,
}

/// Crash-recovery receipt for one repository/PR identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueueAbsentRecoveryRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// Canonical repository identity.
    pub repo: String,
    /// Pull request number.
    pub pr: u64,
    /// Ship-state attempt bound to this generation.
    pub attempt: u32,
    /// Exact stored head branch.
    pub branch: String,
    /// Exact stored base branch.
    pub base_branch: String,
    /// Exact stored head SHA.
    pub head_sha: String,
    /// Preserved work item used as immutable source.
    pub source_job_id: String,
    /// Deterministic replacement job for crash replay.
    pub replacement_job_id: String,
    /// Fresh unpredictable recovery generation.
    pub generation: String,
    /// Typed lifecycle status.
    pub status: QueueAbsentRecoveryStatus,
    /// Bounded fail-closed or needs-agent explanation.
    pub detail: Option<String>,
    /// Last durable transition time.
    pub updated_at: DateTime<Utc>,
}

/// One sweep summary. Skips are intentionally counted but do not mutate ship
/// state or queue ownership.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueueAbsentRecoveryReport {
    /// Exact replacements inserted by this pass.
    pub enqueued: Vec<(String, u64, String)>,
    /// Typed needs-agent routes emitted by this pass.
    pub needs_agent: Vec<(String, u64, String)>,
    /// Candidates made ineligible by a concurrent/newer owner.
    pub skipped: usize,
}

/// Run one recovery sweep. Disabled by default and fail-closed on every
/// unavailable authority boundary.
#[must_use]
pub fn recover_queue_absent_ships(
    state_dir: &Path,
    mode: RuntimeMode,
    global_dir: &Path,
    config: &LoadedConfig,
) -> QueueAbsentRecoveryReport {
    let fetch_global_dir = global_dir.to_path_buf();
    recover_queue_absent_ships_with(
        state_dir,
        mode,
        global_dir,
        config,
        move |repo, pr, cwd| {
            let config =
                LoadedConfig::load_from_cwd_with_global_dir(mode, cwd, fetch_global_dir.clone())?;
            let client = GhClient::from_loaded_config(&config)?;
            crate::wait_transport::fetch_pr_snapshot_with_client(
                &client,
                repo,
                pr,
                cwd,
                Duration::from_secs(15),
            )
        },
        |_| Ok(()),
    )
}

fn recover_queue_absent_ships_with<F, C>(
    state_dir: &Path,
    mode: RuntimeMode,
    global_dir: &Path,
    config: &LoadedConfig,
    mut fetch_pr: F,
    mut after_claim: C,
) -> QueueAbsentRecoveryReport
where
    F: FnMut(&str, u64, &Path) -> Result<Option<Value>, Box<dyn std::error::Error>>,
    C: FnMut(&QueueAbsentRecoveryRecord) -> Result<(), String>,
{
    let mut report = QueueAbsentRecoveryReport::default();
    if !queue_absent_recovery_enabled(config) {
        return report;
    }
    let Ok(store) = ShipStateStore::new(state_dir.join("ship")) else {
        return report;
    };
    for state in store.list_active() {
        if ship_terminal_verdict(&state).is_some() {
            continue;
        }
        let Some(repo_path) = queue_absent_repo_path(config, &state.repo) else {
            let reason = "registered repo path is missing".to_owned();
            let _ = persist_needs_agent(&store, state_dir, &state, &reason);
            report.needs_agent.push((state.repo, state.pr, reason));
            continue;
        };
        match recover_one(
            &store,
            state_dir,
            mode,
            global_dir,
            &state,
            &repo_path,
            &mut fetch_pr,
            &mut after_claim,
        ) {
            Ok(Some(job_id)) => report.enqueued.push((state.repo, state.pr, job_id)),
            Ok(None) => report.skipped += 1,
            Err(reason) => report.needs_agent.push((state.repo, state.pr, reason)),
        }
    }
    report
}

#[allow(clippy::too_many_arguments)]
fn recover_one<F, C>(
    store: &ShipStateStore,
    state_dir: &Path,
    mode: RuntimeMode,
    global_dir: &Path,
    snapshot: &ShipState,
    registered_repo_path: &Path,
    fetch_pr: &mut F,
    after_claim: &mut C,
) -> Result<Option<String>, String>
where
    F: FnMut(&str, u64, &Path) -> Result<Option<Value>, Box<dyn std::error::Error>>,
    C: FnMut(&QueueAbsentRecoveryRecord) -> Result<(), String>,
{
    let repo = snapshot.repo.clone();
    let pr = snapshot.pr;
    store
        .with_pr_state_scoped_locked(&repo, pr, |current| {
            let Some(state) = current.as_ref() else {
                return Ok(None);
            };
            if state != snapshot || ship_terminal_verdict(state).is_some() {
                return Ok(None);
            }
            let result = recover_locked(
                state_dir,
                mode,
                global_dir,
                state,
                registered_repo_path,
                fetch_pr,
                after_claim,
            );
            if let Err(reason) = &result {
                save_needs_agent(state_dir, state, reason).map_err(
                    |error| -> Box<dyn std::error::Error> { std::io::Error::other(error).into() },
                )?;
            }
            result.map_err(|reason| -> Box<dyn std::error::Error> {
                std::io::Error::other(reason).into()
            })
        })
        .map_err(|error| error.to_string())
}

fn recover_locked<F, C>(
    state_dir: &Path,
    mode: RuntimeMode,
    global_dir: &Path,
    state: &ShipState,
    registered_repo_path: &Path,
    fetch_pr: &mut F,
    after_claim: &mut C,
) -> Result<Option<String>, String>
where
    F: FnMut(&str, u64, &Path) -> Result<Option<Value>, Box<dyn std::error::Error>>,
    C: FnMut(&QueueAbsentRecoveryRecord) -> Result<(), String>,
{
    let canonical_path = fs::canonicalize(registered_repo_path)
        .map_err(|error| format!("registered repo path is unavailable: {error}"))?;
    let repo_config = load_recovery_config(mode, &canonical_path, global_dir)?;

    let request_store = QueueRequestStore::new(state_dir).map_err(|error| error.to_string())?;
    let envelopes = request_store
        .list()
        .map_err(|error| format!("durable work provenance is unreadable: {error}"))?;
    let mut matching = envelopes
        .iter()
        .filter(|envelope| envelope_matches_state(envelope, state))
        .collect::<Vec<_>>();
    matching.sort_by_key(|envelope| envelope.created_at);

    let record_path = recovery_record_path(state_dir, &state.repo, state.pr);
    let existing = load_record(&record_path)?;
    let existing = existing.filter(|record| record_matches_state(record, state));
    if recovery_is_fenced(existing.as_ref()) {
        return Ok(None);
    }
    let source = if let Some(record) = &existing {
        envelopes
            .iter()
            .find(|envelope| envelope.job_id == record.source_job_id)
            .ok_or_else(|| "claimed durable work item disappeared; needs-agent".to_owned())?
    } else {
        match matching.as_slice() {
            [source] => *source,
            [] => return Err("no exact preserved work item; needs-agent".to_owned()),
            _ => {
                return Err(
                    "multiple preserved work items make ownership ambiguous; needs-agent"
                        .to_owned(),
                );
            }
        }
    };
    validate_source(source, state, &canonical_path, &repo_config)?;
    ensure_source_has_no_outcome(state_dir, &source.job_id)?;
    if finalize_claimed_replacement_outcome(state_dir, &record_path, existing.as_ref())? {
        return Ok(None);
    }

    let mut queue = Queue::new(state_dir).map_err(|error| error.to_string())?;
    let jobs = queue.get_all().map_err(|error| error.to_string())?;
    let envelope_by_id = envelopes
        .iter()
        .map(|item| (item.job_id.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    if let Some(record) = existing.as_ref()
        && record.status == QueueAbsentRecoveryStatus::Claimed
        && let Some(job) = jobs.iter().find(|job| job.id == record.replacement_job_id)
    {
        let envelope = envelope_by_id
            .get(job.id.as_str())
            .ok_or_else(|| "claimed replacement has no durable work envelope".to_owned())?;
        validate_replacement(job, envelope, record, source, state)?;
        let mut record = record.clone();
        record.status = QueueAbsentRecoveryStatus::Enqueued;
        record.updated_at = Utc::now();
        save_record(&record_path, &record)?;
        return Ok(None);
    }
    if jobs.iter().any(|job| job.id == source.job_id) {
        return Ok(None);
    }
    if jobs.iter().any(|job| {
        matches!(job.status, JobStatus::Pending | JobStatus::Running)
            && envelope_by_id
                .get(job.id.as_str())
                .is_some_and(|envelope| envelope_owns_pr(envelope, state))
    }) {
        return Ok(None);
    }
    if jobs.iter().any(|job| {
        job.created_at > source.created_at
            && envelope_by_id
                .get(job.id.as_str())
                .is_some_and(|envelope| envelope_owns_pr(envelope, state))
    }) {
        return Ok(None);
    }
    ensure_no_worker_receipt(state_dir, state, &envelope_by_id)?;

    let live = fetch_pr(&state.repo, state.pr, &canonical_path)
        .map_err(|error| format!("GitHub PR verification unavailable: {error}"))?
        .ok_or_else(|| "GitHub PR verification unavailable".to_owned())?;
    validate_live_pr(&live, state)?;

    claim_and_enqueue(
        &mut queue,
        &request_store,
        state,
        source,
        existing,
        &record_path,
        after_claim,
    )
}

fn ensure_source_has_no_outcome(state_dir: &Path, source_job_id: &str) -> Result<(), String> {
    if outcome_presence(state_dir, source_job_id)? {
        Err("source has a durable terminal outcome; replay refused".to_owned())
    } else {
        Ok(())
    }
}

fn recovery_is_fenced(record: Option<&QueueAbsentRecoveryRecord>) -> bool {
    record.is_some_and(|record| record.status == QueueAbsentRecoveryStatus::NeedsAgent)
}

fn load_recovery_config(
    mode: RuntimeMode,
    repo_path: &Path,
    global_dir: &Path,
) -> Result<LoadedConfig, String> {
    let config =
        LoadedConfig::load_from_cwd_with_global_dir(mode, repo_path, global_dir.to_path_buf())
            .map_err(|error| format!("registered repo config is unavailable: {error}"))?;
    if config.get_str("github.auth.source") != Some("command") {
        return Err("registered repo lacks unattended command authentication".to_owned());
    }
    Ok(config)
}

fn outcome_presence(state_dir: &Path, job_id: &str) -> Result<bool, String> {
    QueueOutcomeStore::new(state_dir)
        .map_err(|error| error.to_string())?
        .load(job_id)
        .map(|outcome| outcome.is_some())
        .map_err(|error| format!("job outcome is unreadable; replay refused: {error}"))
}

fn finalize_claimed_replacement_outcome(
    state_dir: &Path,
    record_path: &Path,
    record: Option<&QueueAbsentRecoveryRecord>,
) -> Result<bool, String> {
    let Some(record) = record.filter(|record| {
        record.status == QueueAbsentRecoveryStatus::Claimed && !record.replacement_job_id.is_empty()
    }) else {
        return Ok(false);
    };
    if !outcome_presence(state_dir, &record.replacement_job_id)? {
        return Ok(false);
    }
    let mut finalized = record.clone();
    finalized.status = QueueAbsentRecoveryStatus::Enqueued;
    finalized.updated_at = Utc::now();
    save_record(record_path, &finalized)?;
    Ok(true)
}

fn claim_and_enqueue<C>(
    queue: &mut Queue,
    request_store: &QueueRequestStore,
    state: &ShipState,
    source: &QueuedExecutionEnvelope,
    existing: Option<QueueAbsentRecoveryRecord>,
    record_path: &Path,
    after_claim: &mut C,
) -> Result<Option<String>, String>
where
    C: FnMut(&QueueAbsentRecoveryRecord) -> Result<(), String>,
{
    let mut record = existing.unwrap_or_else(|| new_record(state, source));
    if record.status == QueueAbsentRecoveryStatus::Enqueued {
        return Err("prior recovered generation is absent; needs-agent".to_owned());
    }
    save_record(record_path, &record)?;
    after_claim(&record)?;

    let QueuedExecutionRequest::Ship(request) = &source.request else {
        return Err("preserved work item is not a ship request".to_owned());
    };
    let mut replacement = Job::create(
        state.head_sha.clone(),
        state.branch.clone(),
        source.resource_plan.targets.clone(),
        request.mode,
        request.priority,
    )
    .with_kind(JobKind::Ship)
    .with_workload_scope(source.workload_scope());
    replacement.id.clone_from(&record.replacement_job_id);
    replacement.created_at = record.updated_at;
    let mut replacement_envelope = source.clone();
    replacement_envelope
        .job_id
        .clone_from(&record.replacement_job_id);
    replacement_envelope.created_at = record.updated_at;
    request_store
        .save_durable(&replacement_envelope)
        .map_err(|error| error.to_string())?;
    let enqueue = queue
        .enqueue_recovery_if_unowned(replacement, source.created_at)
        .map_err(|error| error.to_string())?;
    let existed = enqueue == RecoveryEnqueue::Existing;
    match enqueue {
        RecoveryEnqueue::Inserted | RecoveryEnqueue::Existing => {}
        RecoveryEnqueue::OwnedBy(owner_id) => {
            record.status = QueueAbsentRecoveryStatus::NeedsAgent;
            record.detail = Some(format!(
                "newer durable owner {owner_id} won recovery admission"
            ));
            record.updated_at = Utc::now();
            save_record(record_path, &record)?;
            return Ok(None);
        }
    }
    if existed {
        let job = queue
            .get(&record.replacement_job_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "existing replacement disappeared during validation".to_owned())?;
        let envelope = request_store
            .load(&record.replacement_job_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "existing replacement has no durable work envelope".to_owned())?;
        validate_replacement(&job, &envelope, &record, source, state)?;
    }
    record.status = QueueAbsentRecoveryStatus::Enqueued;
    record.updated_at = Utc::now();
    save_record(record_path, &record)?;
    Ok(Some(record.replacement_job_id))
}

fn envelope_matches_state(envelope: &QueuedExecutionEnvelope, state: &ShipState) -> bool {
    matches!((&envelope.kind, &envelope.request),
        (QueuedExecutionKind::Ship, QueuedExecutionRequest::Ship(request))
        if request.pr == state.pr
            && canonical_repository(&request.repo) == canonical_repository(&state.repo)
            && request.branch == state.branch
            && request.base_branch == state.base_branch
            && request.sha == state.head_sha)
}

fn envelope_owns_pr(envelope: &QueuedExecutionEnvelope, state: &ShipState) -> bool {
    matches!(&envelope.request, QueuedExecutionRequest::Ship(request)
        if request.pr == state.pr
            && canonical_repository(&request.repo) == canonical_repository(&state.repo))
}

fn validate_source(
    envelope: &QueuedExecutionEnvelope,
    state: &ShipState,
    repo_path: &Path,
    config: &LoadedConfig,
) -> Result<(), String> {
    if !envelope.is_daemon_admissible() || !envelope_matches_state(envelope, state) {
        return Err(
            "preserved work item is not exact daemon-owned ship provenance; needs-agent".to_owned(),
        );
    }
    let request = envelope
        .to_ship_request()
        .map_err(|error| format!("preserved ship request is malformed; needs-agent: {error}"))?;
    if envelope.resource_plan != JobResourcePlan::from_ship_request(repo_path, &request) {
        return Err(
            "preserved scheduler resource plan disagrees with ship request; needs-agent".to_owned(),
        );
    }
    let provenance = envelope
        .provenance
        .as_ref()
        .ok_or_else(|| "preserved work item has no provenance; needs-agent".to_owned())?;
    if provenance.canonical_cwd != repo_path || envelope.cwd != repo_path {
        return Err(
            "preserved checkout does not match registered repo path; needs-agent".to_owned(),
        );
    }
    provenance
        .validate_with_config(repo_path, config)
        .map_err(|error| format!("preserved provenance no longer validates; needs-agent: {error}"))
}

fn validate_replacement(
    job: &Job,
    envelope: &QueuedExecutionEnvelope,
    record: &QueueAbsentRecoveryRecord,
    source: &QueuedExecutionEnvelope,
    state: &ShipState,
) -> Result<(), String> {
    let QueuedExecutionRequest::Ship(request) = &source.request else {
        return Err("source is not a ship request".to_owned());
    };
    let workload_scope = source.workload_scope();
    let exact_job = job.id == record.replacement_job_id
        && job.kind == Some(JobKind::Ship)
        && job.sha == state.head_sha
        && job.branch == state.branch
        && job.mode == request.mode
        && job.priority == request.priority
        && job.target_names == source.resource_plan.targets
        && job.workload_scope.as_deref() == Some(workload_scope.as_str());
    let mut expected = source.clone();
    expected.job_id.clone_from(&record.replacement_job_id);
    expected.created_at = envelope.created_at;
    if !exact_job || envelope != &expected {
        return Err("claimed replacement identity is not exact; needs-agent".to_owned());
    }
    Ok(())
}

fn validate_live_pr(value: &Value, state: &ShipState) -> Result<(), String> {
    let open = value
        .get("state")
        .and_then(Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("open"));
    let head = value.get("headRefOid").and_then(Value::as_str);
    let branch = value.get("headRefName").and_then(Value::as_str);
    let base = value.get("baseRefName").and_then(Value::as_str);
    if !open {
        return Err("PR is not OPEN; recovery refused".to_owned());
    }
    if head != Some(state.head_sha.as_str())
        || branch != Some(state.branch.as_str())
        || base != Some(state.base_branch.as_str())
    {
        return Err("live PR head/base identity drifted; needs-agent".to_owned());
    }
    Ok(())
}

fn ensure_no_worker_receipt(
    state_dir: &Path,
    state: &ShipState,
    envelopes: &BTreeMap<&str, &QueuedExecutionEnvelope>,
) -> Result<(), String> {
    let path = state_dir.join("queue-workers");
    if !path.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let raw = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let receipt: WorkerReceipt = serde_json::from_str(&raw)
            .map_err(|_| "malformed worker receipt makes ownership ambiguous".to_owned())?;
        let Some(envelope) = envelopes.get(receipt.job_id.as_str()) else {
            return Err("worker receipt has no readable work provenance".to_owned());
        };
        if envelope_owns_pr(envelope, state) {
            return Err("worker receipt still owns this ship work".to_owned());
        }
    }
    Ok(())
}

fn new_record(state: &ShipState, source: &QueuedExecutionEnvelope) -> QueueAbsentRecoveryRecord {
    let now = Utc::now();
    let seed = format!(
        "{}:{}:{}:{}:{}",
        state.repo,
        state.pr,
        source.job_id,
        now.timestamp_nanos_opt().unwrap_or_default(),
        std::process::id()
    );
    let generation = format!("{:x}", Sha256::digest(seed.as_bytes()));
    QueueAbsentRecoveryRecord {
        schema_version: RECOVERY_SCHEMA_VERSION,
        repo: state.repo.clone(),
        pr: state.pr,
        attempt: state.attempt,
        branch: state.branch.clone(),
        base_branch: state.base_branch.clone(),
        head_sha: state.head_sha.clone(),
        source_job_id: source.job_id.clone(),
        replacement_job_id: format!("queue-recovery-{}", &generation[..24]),
        generation,
        status: QueueAbsentRecoveryStatus::Claimed,
        detail: None,
        updated_at: now,
    }
}

fn save_needs_agent(state_dir: &Path, state: &ShipState, reason: &str) -> Result<(), String> {
    let path = recovery_record_path(state_dir, &state.repo, state.pr);
    let mut record = load_record(&path)?
        .filter(|record| record_matches_state(record, state))
        .unwrap_or_else(|| QueueAbsentRecoveryRecord {
            schema_version: RECOVERY_SCHEMA_VERSION,
            repo: state.repo.clone(),
            pr: state.pr,
            attempt: state.attempt,
            branch: state.branch.clone(),
            base_branch: state.base_branch.clone(),
            head_sha: state.head_sha.clone(),
            source_job_id: String::new(),
            replacement_job_id: String::new(),
            generation: String::new(),
            status: QueueAbsentRecoveryStatus::NeedsAgent,
            detail: None,
            updated_at: Utc::now(),
        });
    record.status = QueueAbsentRecoveryStatus::NeedsAgent;
    record.detail = Some(reason.to_owned());
    record.updated_at = Utc::now();
    save_record(&path, &record)
}

fn persist_needs_agent(
    store: &ShipStateStore,
    state_dir: &Path,
    snapshot: &ShipState,
    reason: &str,
) -> Result<(), String> {
    store
        .with_pr_state_scoped_locked(&snapshot.repo, snapshot.pr, |current| {
            if current.as_ref() == Some(snapshot) {
                save_needs_agent(state_dir, snapshot, reason).map_err(
                    |error| -> Box<dyn std::error::Error> { std::io::Error::other(error).into() },
                )?;
            }
            Ok(())
        })
        .map_err(|error| error.to_string())
}

fn record_matches_state(record: &QueueAbsentRecoveryRecord, state: &ShipState) -> bool {
    record.pr == state.pr
        && canonical_repository(&record.repo) == canonical_repository(&state.repo)
        && record.attempt == state.attempt
        && record.branch == state.branch
        && record.base_branch == state.base_branch
        && record.head_sha == state.head_sha
}

fn recovery_record_path(state_dir: &Path, repo: &str, pr: u64) -> PathBuf {
    let key = format!(
        "{:x}",
        Sha256::digest(canonical_repository(repo).as_bytes())
    );
    state_dir
        .join("queue")
        .join("recovery")
        .join(format!("{}-{pr}.json", &key[..16]))
}

fn load_record(path: &Path) -> Result<Option<QueueAbsentRecoveryRecord>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let record: QueueAbsentRecoveryRecord =
        serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("malformed recovery claim: {error}"))?;
    if record.schema_version != RECOVERY_SCHEMA_VERSION {
        return Err("unsupported recovery claim schema".to_owned());
    }
    Ok(Some(record))
}

fn save_record(path: &Path, record: &QueueAbsentRecoveryRecord) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "recovery claim path has no parent".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temp = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    serde_json::to_writer_pretty(temp.as_file(), record).map_err(|error| error.to_string())?;
    temp.as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    temp.persist(path).map_err(|error| error.to_string())?;
    crate::log_retention::sync_parent_directory(path).map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::process::Command;

    use crate::config::LoadedConfig;
    use crate::job::{Priority, ValidationMode};
    use crate::queue_request::{ExecutionProvenance, QueuedExecutionOwner};
    use crate::ship::ShipExecutionRequest;

    const REPO: &str = "Generous-Corp/pulp";
    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct Fixture {
        _temp: tempfile::TempDir,
        state_dir: PathBuf,
        global_dir: PathBuf,
        config: LoadedConfig,
        source_job_id: String,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("tempdir");
            let state_dir = temp.path().join("state");
            let global_dir = temp.path().join("global");
            let repo_path = temp.path().join("repo");
            fs::create_dir_all(repo_path.join(".shipyard")).expect("repo config dir");
            run(&repo_path, &["init"]);
            run(&repo_path, &["config", "user.email", "test@example.com"]);
            run(&repo_path, &["config", "user.name", "Test"]);
            fs::write(repo_path.join("tracked"), "one\n").expect("tracked");
            run(&repo_path, &["add", "."]);
            run(&repo_path, &["commit", "-m", "fixture"]);
            run(
                &repo_path,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/Generous-Corp/pulp.git",
                ],
            );
            let actual_sha = output(&repo_path, &["rev-parse", "HEAD"]);
            fs::create_dir_all(&global_dir).expect("global");
            let path = toml::Value::String(repo_path.to_string_lossy().into_owned());
            fs::write(
                global_dir.join("config.toml"),
                format!(
                    "[github.auth]\nsource = \"command\"\ntoken_command = [\"/usr/bin/printf\", \"token\"]\n\
                     [ship_state]\nqueue_absent_recovery = true\n[ship_state.repo_paths]\n\"{REPO}\" = {path}\n"
                ),
            )
            .expect("global config");
            let config = LoadedConfig::load_from_cwd_with_global_dir(
                RuntimeMode::Shipyard,
                &repo_path,
                global_dir.clone(),
            )
            .expect("config");
            let mut state = ShipState::new(42, REPO, "feature", "main", &actual_sha, "policy");
            state.updated_at = Utc::now() - chrono::Duration::hours(24);
            ShipStateStore::new(state_dir.join("ship"))
                .expect("state store")
                .save(&state)
                .expect("state");
            let request = ShipExecutionRequest {
                pr: 42,
                repo: REPO.to_owned(),
                branch: "feature".to_owned(),
                base_branch: "main".to_owned(),
                sha: actual_sha,
                commit_subject: String::new(),
                pr_url: None,
                pr_title: None,
                mode: ValidationMode::Full,
                priority: Priority::Normal,
                warm_disabled: false,
                fail_fast: false,
                resume_from: None,
                advisory_targets: BTreeSet::new(),
                adopt_head: false,
                pr_snapshot_file: None,
                targets: Vec::new(),
            };
            let source_job_id = "lost-source".to_owned();
            let mut envelope = QueuedExecutionEnvelope::from_ship_request(
                source_job_id.clone(),
                &repo_path,
                &request,
            );
            envelope.execution_owner = QueuedExecutionOwner::Daemon;
            envelope.provenance = ExecutionProvenance::capture_with_config(
                &repo_path,
                Some(REPO),
                &request.sha,
                &config,
            );
            envelope.cwd = envelope
                .provenance
                .as_ref()
                .expect("provenance")
                .canonical_cwd
                .clone();
            QueueRequestStore::new(&state_dir)
                .expect("requests")
                .save(&envelope)
                .expect("source envelope");
            Self {
                _temp: temp,
                state_dir,
                global_dir,
                config,
                source_job_id,
            }
        }

        fn live(&self) -> Value {
            let state = ShipStateStore::new(self.state_dir.join("ship"))
                .expect("store")
                .get_scoped(REPO, 42)
                .expect("state");
            serde_json::json!({
                "state": "OPEN", "headRefOid": state.head_sha,
                "headRefName": state.branch, "baseRefName": state.base_branch
            })
        }

        fn sweep<C>(&self, live: Value, after_claim: C) -> QueueAbsentRecoveryReport
        where
            C: FnMut(&QueueAbsentRecoveryRecord) -> Result<(), String>,
        {
            recover_queue_absent_ships_with(
                &self.state_dir,
                RuntimeMode::Shipyard,
                &self.global_dir,
                &self.config,
                move |_, _, _| Ok(Some(live.clone())),
                after_claim,
            )
        }
    }

    fn run(cwd: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .expect("git")
                .success()
        );
    }

    fn output(cwd: &Path, args: &[&str]) -> String {
        String::from_utf8(
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_owned()
    }

    #[test]
    fn exact_absent_work_is_reenqueued_once_and_duplicate_ticks_do_not_duplicate() {
        let fixture = Fixture::new();
        let first = fixture.sweep(fixture.live(), |_| Ok(()));
        assert_eq!(first.enqueued.len(), 1, "{first:?}");
        let second = fixture.sweep(fixture.live(), |_| Ok(()));
        assert!(second.enqueued.is_empty());
        assert_eq!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get_pending()
                .expect("pending")
                .len(),
            1
        );
    }

    #[test]
    fn absent_envelope_sweep_preserves_nonterminal_ship_recovery_authority() {
        let fixture = Fixture::new();
        let request_store = QueueRequestStore::new(&fixture.state_dir).expect("requests");
        let retained = protected_request_job_ids(&fixture.state_dir, &request_store)
            .expect("protected request ids");
        assert_eq!(retained, BTreeSet::from([fixture.source_job_id.clone()]));
        assert!(
            request_store
                .sweep_absent_older_than(&retained, Duration::ZERO)
                .expect("sweep active ship requests")
                .is_empty()
        );
        assert!(
            request_store
                .load(&fixture.source_job_id)
                .expect("load retained source")
                .is_some()
        );

        let state_store = ShipStateStore::new(fixture.state_dir.join("ship")).expect("state store");
        let mut state = state_store.get_scoped(REPO, 42).expect("state");
        state
            .evidence_snapshot
            .insert("mac".to_owned(), "pass".to_owned());
        state_store.save(&state).expect("terminal state");
        let retained = protected_request_job_ids(&fixture.state_dir, &request_store)
            .expect("terminal request ids");
        assert!(retained.is_empty());
        assert_eq!(
            request_store
                .sweep_absent_older_than(&retained, Duration::ZERO)
                .expect("sweep terminal ship requests"),
            vec![fixture.source_job_id]
        );
    }

    #[test]
    fn crash_after_claim_resumes_same_generation() {
        let fixture = Fixture::new();
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fixture.sweep(fixture.live(), |_| panic!("simulated process exit"));
        }));
        assert!(crashed.is_err(), "claim hook was not reached");
        let claim = load_record(&recovery_record_path(&fixture.state_dir, REPO, 42))
            .expect("claim")
            .expect("record");
        assert_eq!(claim.status, QueueAbsentRecoveryStatus::Claimed);
        let replacement = claim.replacement_job_id.clone();
        let second = fixture.sweep(fixture.live(), |_| Ok(()));
        assert_eq!(second.enqueued[0].2, replacement);
    }

    #[test]
    fn needs_agent_is_a_durable_non_runnable_fence() {
        let fixture = Fixture::new();
        let state = ShipStateStore::new(fixture.state_dir.join("ship"))
            .expect("store")
            .get_scoped(REPO, 42)
            .expect("state");
        let source = QueueRequestStore::new(&fixture.state_dir)
            .expect("requests")
            .load(&fixture.source_job_id)
            .expect("load")
            .expect("source");
        let mut record = new_record(&state, &source);
        record.status = QueueAbsentRecoveryStatus::NeedsAgent;
        record.detail = Some("operator decision required".to_owned());
        save_record(&recovery_record_path(&fixture.state_dir, REPO, 42), &record).expect("record");

        let report = fixture.sweep(fixture.live(), |_| panic!("must not reclaim needs-agent"));
        assert!(report.enqueued.is_empty(), "{report:?}");
        assert_eq!(report.skipped, 1);
        assert!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get_all()
                .expect("jobs")
                .is_empty()
        );
        assert_eq!(
            load_record(&recovery_record_path(&fixture.state_dir, REPO, 42))
                .expect("record read")
                .expect("record"),
            record
        );
    }

    #[test]
    fn head_drift_and_github_offline_fail_closed() {
        let fixture = Fixture::new();
        let mut drift = fixture.live();
        drift["headRefOid"] = Value::String(SHA.to_owned());
        assert!(fixture.sweep(drift, |_| Ok(())).enqueued.is_empty());
        let offline = recover_queue_absent_ships_with(
            &fixture.state_dir,
            RuntimeMode::Shipyard,
            &fixture.global_dir,
            &fixture.config,
            |_, _, _| Ok(None),
            |_| Ok(()),
        );
        assert!(offline.enqueued.is_empty());
        assert!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get_all()
                .expect("jobs")
                .is_empty()
        );
    }

    #[test]
    fn malformed_provenance_routes_needs_agent_without_wedging_queue() {
        let fixture = Fixture::new();
        let store = QueueRequestStore::new(&fixture.state_dir).expect("store");
        let mut envelope = store
            .load(&fixture.source_job_id)
            .expect("load")
            .expect("source");
        envelope.provenance = None;
        store.save(&envelope).expect("save malformed");
        let unrelated = Job::create(
            "sha",
            "other",
            vec![],
            ValidationMode::Smoke,
            Priority::Normal,
        );
        Queue::new(&fixture.state_dir)
            .expect("queue")
            .enqueue(unrelated.clone())
            .expect("enqueue");
        let report = fixture.sweep(fixture.live(), |_| Ok(()));
        assert!(report.enqueued.is_empty());
        assert!(!report.needs_agent.is_empty());
        assert!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get(&unrelated.id)
                .expect("get")
                .is_some()
        );
    }

    #[test]
    fn mismatched_source_kind_fails_closed_before_queue_mutation() {
        let fixture = Fixture::new();
        let store = QueueRequestStore::new(&fixture.state_dir).expect("store");
        let mut envelope = store
            .load(&fixture.source_job_id)
            .expect("load")
            .expect("source");
        envelope.kind = QueuedExecutionKind::Run;
        store.save(&envelope).expect("save malformed");

        let report = fixture.sweep(fixture.live(), |_| Ok(()));
        assert!(report.enqueued.is_empty(), "{report:?}");
        assert!(!report.needs_agent.is_empty());
        assert!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get_all()
                .expect("jobs")
                .is_empty()
        );
    }

    #[test]
    fn mismatched_source_resource_plan_fails_closed_before_queue_mutation() {
        let fixture = Fixture::new();
        let store = QueueRequestStore::new(&fixture.state_dir).expect("store");
        let mut envelope = store
            .load(&fixture.source_job_id)
            .expect("load")
            .expect("source");
        envelope.resource_plan.targets.push("hostile".to_owned());
        store.save(&envelope).expect("save malformed");

        let report = fixture.sweep(fixture.live(), |_| Ok(()));
        assert!(report.enqueued.is_empty(), "{report:?}");
        assert!(
            report
                .needs_agent
                .iter()
                .any(|(_, _, reason)| reason.contains("resource plan"))
        );
        assert!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get_all()
                .expect("jobs")
                .is_empty()
        );
    }

    #[test]
    fn new_worker_winning_after_claim_blocks_recovery_writer() {
        let fixture = Fixture::new();
        let state_dir = fixture.state_dir.clone();
        let source_id = fixture.source_job_id.clone();
        let report = fixture.sweep(fixture.live(), move |_| {
            let store = QueueRequestStore::new(&state_dir).map_err(|error| error.to_string())?;
            let source = store
                .load(&source_id)
                .map_err(|error| error.to_string())?
                .ok_or("source")?;
            let mut owner = Job::create(
                "sha",
                "feature",
                vec![],
                ValidationMode::Full,
                Priority::Normal,
            )
            .with_kind(JobKind::Ship)
            .with_workload_scope(source.workload_scope());
            owner.id = "new-owner".to_owned();
            let mut owner_envelope = source.clone();
            owner_envelope.job_id = owner.id.clone();
            store
                .save(&owner_envelope)
                .map_err(|error| error.to_string())?;
            Queue::new(&state_dir)
                .map_err(|error| error.to_string())?
                .enqueue(owner)
                .map_err(|error| error.to_string())?;
            Ok(())
        });
        assert!(report.enqueued.is_empty());
        let jobs = Queue::new(&fixture.state_dir)
            .expect("queue")
            .get_pending()
            .expect("pending");
        assert_eq!(jobs.len(), 1, "{report:?}");
        assert_eq!(jobs[0].id, "new-owner");
        let record = load_record(&recovery_record_path(&fixture.state_dir, REPO, 42))
            .expect("record read")
            .expect("record");
        assert_eq!(record.status, QueueAbsentRecoveryStatus::NeedsAgent);
        assert!(
            record
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("new-owner"))
        );
        assert!(
            fixture
                .sweep(fixture.live(), |_| panic!("fenced claim must not replay"))
                .enqueued
                .is_empty()
        );
    }

    #[test]
    fn disabled_kill_switch_and_existing_worker_receipt_are_no_ops() {
        let fixture = Fixture::new();
        let disabled = LoadedConfig {
            data: toml::Table::new(),
            global_dir: fixture.global_dir.clone(),
            project_dir: None,
            local_dir: None,
            local_overlay_source: crate::config::LocalOverlaySource::None,
        };
        let off = recover_queue_absent_ships_with(
            &fixture.state_dir,
            RuntimeMode::Shipyard,
            &fixture.global_dir,
            &disabled,
            |_, _, _| panic!("disabled recovery must not contact GitHub"),
            |_| Ok(()),
        );
        assert_eq!(off, QueueAbsentRecoveryReport::default());

        let worker_dir = fixture.state_dir.join("queue-workers");
        fs::create_dir_all(&worker_dir).expect("worker dir");
        fs::write(
            worker_dir.join(format!("{}.json", fixture.source_job_id)),
            serde_json::to_vec(&WorkerReceipt {
                job_id: fixture.source_job_id.clone(),
                generation: "live-generation".to_owned(),
                pid: std::process::id(),
                started_at: Utc::now(),
            })
            .expect("receipt json"),
        )
        .expect("receipt");
        let blocked = fixture.sweep(fixture.live(), |_| Ok(()));
        assert!(blocked.enqueued.is_empty());
        assert!(
            blocked
                .needs_agent
                .iter()
                .any(|(_, _, detail)| detail.contains("worker receipt"))
        );
    }

    #[test]
    fn terminal_source_job_is_not_replayed() {
        let fixture = Fixture::new();
        let source = QueueRequestStore::new(&fixture.state_dir)
            .expect("store")
            .load(&fixture.source_job_id)
            .expect("load")
            .expect("source");
        let mut job = Job::create(
            "sha",
            "feature",
            vec![],
            ValidationMode::Full,
            Priority::Normal,
        )
        .with_kind(JobKind::Ship)
        .with_workload_scope(source.workload_scope());
        job.id.clone_from(&fixture.source_job_id);
        job = job.start().expect("start").complete().expect("complete");
        Queue::new(&fixture.state_dir)
            .expect("queue")
            .enqueue(job)
            .expect("terminal source");
        assert!(
            fixture
                .sweep(fixture.live(), |_| Ok(()))
                .enqueued
                .is_empty()
        );
    }

    #[test]
    fn trimmed_terminal_source_with_durable_outcome_is_not_replayed() {
        let fixture = Fixture::new();
        let state = ShipStateStore::new(fixture.state_dir.join("ship"))
            .expect("store")
            .get_scoped(REPO, 42)
            .expect("state");
        QueueOutcomeStore::new(&fixture.state_dir)
            .expect("outcomes")
            .save(&crate::queue_request::QueuedExecutionOutcome::ship(
                &fixture.source_job_id,
                42,
                state,
                false,
            ))
            .expect("outcome");
        let report = fixture.sweep(fixture.live(), |_| Ok(()));
        assert!(report.enqueued.is_empty());
        assert!(
            report
                .needs_agent
                .iter()
                .any(|(_, _, detail)| detail.contains("terminal outcome"))
        );
    }

    #[test]
    fn trimmed_replacement_with_durable_outcome_finalizes_claim_without_replay() {
        let fixture = Fixture::new();
        let state = ShipStateStore::new(fixture.state_dir.join("ship"))
            .expect("store")
            .get_scoped(REPO, 42)
            .expect("state");
        let source = QueueRequestStore::new(&fixture.state_dir)
            .expect("requests")
            .load(&fixture.source_job_id)
            .expect("load")
            .expect("source");
        let record = new_record(&state, &source);
        save_record(&recovery_record_path(&fixture.state_dir, REPO, 42), &record).expect("claim");
        QueueOutcomeStore::new(&fixture.state_dir)
            .expect("outcomes")
            .save(&crate::queue_request::QueuedExecutionOutcome::ship(
                &record.replacement_job_id,
                42,
                state,
                false,
            ))
            .expect("replacement outcome");

        let report = fixture.sweep(fixture.live(), |_| Ok(()));
        assert!(report.enqueued.is_empty(), "{report:?}");
        assert!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get_all()
                .expect("jobs")
                .is_empty()
        );
        let finalized = load_record(&recovery_record_path(&fixture.state_dir, REPO, 42))
            .expect("record read")
            .expect("record");
        assert_eq!(finalized.status, QueueAbsentRecoveryStatus::Enqueued);
    }

    #[test]
    fn crash_after_queue_commit_finalizes_observed_exact_replacement() {
        let fixture = Fixture::new();
        let state_dir = fixture.state_dir.clone();
        let source_id = fixture.source_job_id.clone();
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fixture.sweep(fixture.live(), move |record| {
                let store = QueueRequestStore::new(&state_dir).expect("store");
                let source = store.load(&source_id).expect("load").expect("source");
                let QueuedExecutionRequest::Ship(request) = &source.request else {
                    panic!("ship request");
                };
                let mut job = Job::create(
                    request.sha.clone(),
                    request.branch.clone(),
                    source.resource_plan.targets.clone(),
                    request.mode,
                    request.priority,
                )
                .with_kind(JobKind::Ship)
                .with_workload_scope(source.workload_scope());
                job.id.clone_from(&record.replacement_job_id);
                let mut envelope = source.clone();
                envelope.job_id.clone_from(&record.replacement_job_id);
                store.save(&envelope).expect("replacement envelope");
                Queue::new(&state_dir)
                    .expect("queue")
                    .enqueue_recovery_if_unowned(job, source.created_at)
                    .expect("enqueue");
                panic!("crash after queue commit");
            });
        }));
        assert!(crashed.is_err());
        assert!(
            fixture
                .sweep(fixture.live(), |_| Ok(()))
                .enqueued
                .is_empty()
        );
        let record = load_record(&recovery_record_path(&fixture.state_dir, REPO, 42))
            .expect("record read")
            .expect("record");
        assert_eq!(record.status, QueueAbsentRecoveryStatus::Enqueued);
        assert_eq!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get_all()
                .expect("jobs")
                .len(),
            1
        );
    }

    #[test]
    fn same_id_but_mismatched_replacement_routes_needs_agent() {
        let fixture = Fixture::new();
        let state = ShipStateStore::new(fixture.state_dir.join("ship"))
            .expect("store")
            .get_scoped(REPO, 42)
            .expect("state");
        let request_store = QueueRequestStore::new(&fixture.state_dir).expect("requests");
        let source = request_store
            .load(&fixture.source_job_id)
            .expect("load")
            .expect("source");
        let record = new_record(&state, &source);
        save_record(&recovery_record_path(&fixture.state_dir, REPO, 42), &record).expect("claim");
        let mut hostile = Job::create(
            "wrong-sha",
            state.branch.clone(),
            source.resource_plan.targets.clone(),
            ValidationMode::Full,
            Priority::Normal,
        )
        .with_kind(JobKind::Ship)
        .with_workload_scope(source.workload_scope());
        hostile.id.clone_from(&record.replacement_job_id);
        let mut envelope = source;
        envelope.job_id.clone_from(&record.replacement_job_id);
        request_store.save(&envelope).expect("hostile envelope");
        Queue::new(&fixture.state_dir)
            .expect("queue")
            .enqueue(hostile)
            .expect("hostile job");
        let report = fixture.sweep(fixture.live(), |_| Ok(()));
        assert!(report.enqueued.is_empty());
        assert!(
            report
                .needs_agent
                .iter()
                .any(|(_, _, detail)| detail.contains("replacement identity"))
        );
    }

    #[test]
    fn stale_claim_from_prior_attempt_does_not_poison_current_attempt() {
        let fixture = Fixture::new();
        let state = ShipStateStore::new(fixture.state_dir.join("ship"))
            .expect("store")
            .get_scoped(REPO, 42)
            .expect("state");
        let source = QueueRequestStore::new(&fixture.state_dir)
            .expect("requests")
            .load(&fixture.source_job_id)
            .expect("load")
            .expect("source");
        let mut prior_claim = new_record(&state, &source);
        prior_claim.attempt = state.attempt + 1;
        let stale_generation = prior_claim.generation.clone();
        save_record(
            &recovery_record_path(&fixture.state_dir, REPO, 42),
            &prior_claim,
        )
        .expect("stale claim");
        assert_eq!(fixture.sweep(fixture.live(), |_| Ok(())).enqueued.len(), 1);
        let current = load_record(&recovery_record_path(&fixture.state_dir, REPO, 42))
            .expect("record read")
            .expect("record");
        assert_ne!(current.generation, stale_generation);
        assert_eq!(current.attempt, state.attempt);
    }

    #[test]
    fn needs_agent_replaces_a_prior_attempt_claim() {
        let fixture = Fixture::new();
        let state = ShipStateStore::new(fixture.state_dir.join("ship"))
            .expect("store")
            .get_scoped(REPO, 42)
            .expect("state");
        let request_store = QueueRequestStore::new(&fixture.state_dir).expect("requests");
        let mut source = request_store
            .load(&fixture.source_job_id)
            .expect("load")
            .expect("source");
        let mut prior_claim = new_record(&state, &source);
        prior_claim.attempt += 1;
        save_record(
            &recovery_record_path(&fixture.state_dir, REPO, 42),
            &prior_claim,
        )
        .expect("prior claim");
        source.provenance = None;
        request_store.save(&source).expect("malformed source");
        let report = fixture.sweep(fixture.live(), |_| Ok(()));
        assert!(!report.needs_agent.is_empty());
        let current = load_record(&recovery_record_path(&fixture.state_dir, REPO, 42))
            .expect("record read")
            .expect("record");
        assert_eq!(current.status, QueueAbsentRecoveryStatus::NeedsAgent);
        assert_eq!(current.attempt, state.attempt);
        assert!(record_matches_state(&current, &state));
    }
}
