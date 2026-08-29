use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{CliFailure, cli::QueuePriority};
use crate::config::LoadedConfig;
use crate::evidence::EvidenceStore;
use crate::executor::dispatch::{ExecutorDispatcher, resolve_targets};
use crate::host_pool::{HostPoolLeaseStore, default_lease_path};
use crate::identity::RuntimeMode;
use crate::job::{CancellationProof, Job, JobStatus, Priority, ValidationMode};
use crate::output::write_json_envelope;
use crate::queue::{Queue, QueueError};
use crate::queue_request::{QueueRequestStore, QueuedExecutionOwner, QueuedExecutionRequest};
use crate::queue_scheduler::AlreadyMergedObserver;
use crate::ship::persist_terminal_outcome;

impl From<QueueError> for CliFailure {
    fn from(error: QueueError) -> Self {
        Self::new(1, error.to_string())
    }
}

impl From<serde_json::Error> for CliFailure {
    fn from(error: serde_json::Error) -> Self {
        Self::new(1, error.to_string())
    }
}

pub(super) fn status_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut queue = open_queue(state_dir)?;
    let active_runs = queue.get_running()?;
    let active = active_runs.first();
    let pending = queue.pending_count()?;
    let recent = queue.get_recent(5)?;
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let targets = target_statuses(&config)?;
    let orphaned = collect_orphaned_ship_states(state_dir, &config);

    if json_mode {
        let mut data = BTreeMap::new();
        data.insert(
            "queue".to_owned(),
            json!({
                "pending": pending,
                "running": active_runs.len(),
                "completed_recent": recent.len(),
            }),
        );
        if let Some(active) = active.as_ref() {
            data.insert("active_run".to_owned(), active.to_json_value());
        }
        data.insert("active_runs".to_owned(), jobs_value(&active_runs)?);
        data.insert("targets".to_owned(), serde_json::to_value(targets)?);
        data.insert(
            "orphaned_ship_states".to_owned(),
            Value::Array(
                orphaned
                    .iter()
                    .map(|(repo, pr, report)| {
                        json!({
                            "repo": repo,
                            "pr": pr,
                            "stalled_minutes": report.stalled_minutes,
                            "evidence": report.evidence.as_str(),
                        })
                    })
                    .collect(),
            ),
        );
        write_json_envelope(stdout, "status", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        write_status_human(
            stdout,
            active_runs.len(),
            pending,
            &recent,
            &targets,
            &orphaned,
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Classify every active ship-state against the queue for orphan reporting.
/// Best-effort and strictly read-only: only reads the ship-state store when it
/// already exists (so `status` never materializes state in a fresh directory),
/// and yields nothing on any read failure rather than failing `status`.
fn collect_orphaned_ship_states(
    state_dir: &Path,
    config: &LoadedConfig,
) -> Vec<(String, u64, crate::ship_liveness::OrphanReport)> {
    let ship_dir = state_dir.join("ship");
    if !ship_dir.is_dir() {
        return Vec::new();
    }
    let Ok(store) = crate::ship_state::ShipStateStore::new(ship_dir) else {
        return Vec::new();
    };
    let stale_after = crate::ship_liveness::orphan_stale_after(config);
    let now = chrono::Utc::now();
    crate::ship_liveness::with_liveness_context(state_dir, stale_after, |liveness| {
        crate::ship_liveness::collect_orphans(&store, liveness, now)
    })
}

pub(super) fn evidence_command<W: Write>(
    branch: Option<String>,
    cwd: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let branch = branch
        .or_else(|| current_git_branch(cwd))
        .unwrap_or_else(|| "main".to_owned());
    let store = EvidenceStore::new(state_dir.join("evidence"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut records = BTreeMap::new();
    if let Some(repository) = super::branch_cmd::detect_repo_from_remote(cwd, None) {
        records.extend(store.get_branch_scoped(
            &crate::evidence::repository_evidence_scope(&repository),
            &branch,
        ));
        for (target, record) in store.get_branch_scoped_prefix(
            &crate::evidence::repository_ship_evidence_scope_prefix(&repository),
            &branch,
        ) {
            if records
                .get(&target)
                .is_none_or(|existing| record.completed_at > existing.completed_at)
            {
                records.insert(target, record);
            }
        }
    }
    for (target, record) in
        store.get_branch_scoped(&crate::evidence::run_evidence_scope(cwd), &branch)
    {
        if records
            .get(&target)
            .is_none_or(|existing| record.completed_at > existing.completed_at)
        {
            records.insert(target, record);
        }
    }
    // Legacy branch-only records remain visible until every machine has
    // emitted scoped evidence at least once. They are display-only here and
    // never satisfy scoped reuse or exact-head gates.
    for (target, record) in store.get_branch(&branch) {
        records.entry(target).or_insert(record);
    }
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("branch".to_owned(), Value::String(branch.clone()));
        data.insert("evidence".to_owned(), serde_json::to_value(&records)?);
        write_json_envelope(stdout, "evidence", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if records.is_empty() {
        writeln!(stdout, "No evidence for {branch}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "Evidence for {branch}:")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for (target, record) in records {
            writeln!(
                stdout,
                "  {target}: {} {} {}",
                record.status, record.sha, record.completed_at
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn logs_command<W: Write>(
    job_id: &str,
    target: Option<String>,
    state_dir: &Path,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut queue = open_queue(state_dir)?;
    let Some(job) = queue.get(job_id)? else {
        let target = target.ok_or_else(|| {
            CliFailure::new(
                1,
                format!("Job {job_id} is retained; specify --target to read its log"),
            )
        })?;
        if !is_plain_component(job_id) || !is_plain_component(&target) {
            return Err(CliFailure::new(2, "target must be one path component"));
        }
        let job_dir = state_dir.join("logs").join(job_id);
        let job_kind = fs::symlink_metadata(&job_dir)
            .ok()
            .map(|value| value.file_type());
        if !job_kind.is_some_and(|kind| kind.is_dir() && !kind.is_symlink()) {
            return Err(CliFailure::new(1, format!("Job {job_id} not found")));
        }
        write_retained_target_logs(stdout, &job_dir, &target)?;
        return Ok(ExitCode::SUCCESS);
    };
    if let Some(target) = target {
        let result = job
            .results
            .get(&target)
            .ok_or_else(|| CliFailure::new(1, format!("No log for target {target}")))?;
        let log_path = result
            .log_path
            .as_ref()
            .ok_or_else(|| CliFailure::new(1, format!("No log for target {target}")))?;
        write_log(stdout, log_path)?;
        return Ok(ExitCode::SUCCESS);
    }

    for name in &job.target_names {
        if let Some(result) = job.results.get(name)
            && let Some(log_path) = result.log_path.as_ref()
        {
            writeln!(stdout, "\n--- {name} ---")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            write_log(stdout, log_path)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn is_plain_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && Path::new(value).file_name().and_then(|name| name.to_str()) == Some(value)
}

fn write_retained_target_logs<W: Write>(
    stdout: &mut W,
    job_dir: &Path,
    target: &str,
) -> Result<(), CliFailure> {
    let base_name = format!("{target}.log");
    let mut logical_logs = BTreeMap::new();
    for entry in fs::read_dir(job_dir).map_err(|error| CliFailure::new(1, error.to_string()))? {
        let entry = entry.map_err(|error| CliFailure::new(1, error.to_string()))?;
        let kind = entry
            .file_type()
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if !kind.is_file() || kind.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some((order, root_name)) = retained_log_root(&name, &base_name) {
            logical_logs.insert(order, job_dir.join(root_name));
        }
    }
    if logical_logs.is_empty() {
        return Err(CliFailure::new(
            1,
            format!("No retained log for target {target}"),
        ));
    }
    for path in logical_logs.into_values() {
        write_log(stdout, &path.to_string_lossy())?;
    }
    Ok(())
}

fn retained_log_root(name: &str, base_name: &str) -> Option<([u32; 2], String)> {
    let without_gzip = name.strip_suffix(".gz").unwrap_or(name);
    let (root, _) = without_gzip
        .rsplit_once('.')
        .filter(|(_, suffix)| suffix.parse::<usize>().is_ok())
        .unwrap_or((without_gzip, ""));
    let mut suffix = root.strip_prefix(base_name)?;
    let mut order = [0, 0];
    while !suffix.is_empty() {
        let (slot, offset, rest) = if let Some(rest) = suffix.strip_prefix(".retry") {
            (0, 0, rest)
        } else {
            (1, 1, suffix.strip_prefix(".attempt-")?)
        };
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let index = rest[..digits].parse::<u32>().ok()?;
        if order[slot] != 0 || (slot == 0 && order[1] != 0) {
            return None;
        }
        order[slot] = index.checked_add(offset)?;
        suffix = &rest[digits..];
    }
    Some((order, root.to_owned()))
}

pub(super) fn cancel_command<W: Write>(
    job_id: &str,
    reason: Option<&str>,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut queue = open_queue(state_dir)?;
    let host = std::env::var("WHENCE_HOST")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_owned());
    let agent = std::env::var("WHENCE_AGENT").unwrap_or_else(|_| "unknown-agent".to_owned());
    let reason = reason.map_or_else(
        || {
            format!(
                "Manual cancellation via shipyard cancel (host={host}, agent={agent}, pid={})",
                std::process::id()
            )
        },
        str::to_owned,
    );
    let cancelled = queue
        .request_cancel(job_id, Some(reason))?
        .ok_or_else(|| CliFailure::new(1, format!("Job {job_id} not found")))?;
    if json_mode {
        write_job_envelope(stdout, "cancel", &cancelled)?;
    } else {
        writeln!(stdout, "Cancelled {job_id}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

const ORPHAN_RECONCILIATION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum OrphanReconciliationPhase {
    Prepared,
    Finalized,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct OrphanReconciliationRecord {
    schema_version: u32,
    job_id: String,
    job_sha256: String,
    request_sha256: String,
    proof: CancellationProof,
    related_processes: Vec<String>,
    prepared_at: chrono::DateTime<Utc>,
    phase: OrphanReconciliationPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finalized_at: Option<chrono::DateTime<Utc>>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn reconcile_orphan_command<W: Write>(
    job_id: &str,
    expected_head: Option<&str>,
    expected_request_sha256: Option<&str>,
    expected_job_sha256: Option<&str>,
    apply: bool,
    confirm_no_worker_tree: bool,
    _mode: crate::identity::RuntimeMode,
    global_dir: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    reconcile_orphan_command_with(
        job_id,
        expected_head,
        expected_request_sha256,
        expected_job_sha256,
        apply,
        confirm_no_worker_tree,
        global_dir,
        state_dir,
        json_mode,
        stdout,
        |job, request_store, global_dir| {
            let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            let mut observer = AlreadyMergedObserver::from_config(&config);
            Ok(observer
                .observe_running(std::slice::from_ref(job), request_store, global_dir, None)
                .into_iter()
                .next())
        },
        related_process_inventory,
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn reconcile_orphan_command_with<W, O, I>(
    job_id: &str,
    expected_head: Option<&str>,
    expected_request_sha256: Option<&str>,
    expected_job_sha256: Option<&str>,
    apply: bool,
    confirm_no_worker_tree: bool,
    global_dir: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
    mut observe_exact_merge: O,
    mut inventory_related_processes: I,
) -> Result<ExitCode, CliFailure>
where
    W: Write,
    O: FnMut(
        &Job,
        &QueueRequestStore,
        &Path,
    ) -> Result<Option<crate::queue_scheduler::AlreadyMergedCancellation>, CliFailure>,
    I: FnMut(&str, &Path) -> Result<Vec<String>, CliFailure>,
{
    let mut queue = open_queue(state_dir)?;
    let exact_queue = queue.get_all()?;
    let job = exact_queue
        .iter()
        .find(|job| job.id == job_id)
        .cloned()
        .ok_or_else(|| CliFailure::new(1, format!("Job {job_id} not found")))?;
    let request_store =
        QueueRequestStore::new(state_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let request_path = request_store.path_for(job_id);
    let request_bytes = fs::read(&request_path)
        .map_err(|error| CliFailure::new(1, format!("read {}: {error}", request_path.display())))?;
    let request_sha256 = hex::encode(Sha256::digest(&request_bytes));
    let job_sha256 = canonical_job_sha256(&job)?;
    let record_path = orphan_reconciliation_path(state_dir, job_id)?;

    if let Some(record) = load_orphan_reconciliation(&record_path)? {
        let record_matches_queue = orphan_record_matches_queue(&record, &job, &job_sha256);
        if record.schema_version != ORPHAN_RECONCILIATION_SCHEMA_VERSION
            || record.job_id != job_id
            || record.request_sha256 != request_sha256
            || !record_matches_queue
        {
            return Err(CliFailure::new(
                1,
                "orphan reconciliation record contradicts current exact state",
            ));
        }
        if record.phase == OrphanReconciliationPhase::Finalized {
            return render_orphan_reconciliation(
                stdout,
                json_mode,
                &job,
                &record,
                false,
                "already_finalized",
            );
        }
        if job.status == JobStatus::Cancelled {
            require_daemon_stopped(state_dir)?;
            let _drain = queue.acquire_drain_lock()?.ok_or_else(|| {
                CliFailure::new(1, "queue drain lock is owned by another process")
            })?;
            persist_terminal_outcome(&job, state_dir)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            HostPoolLeaseStore::new(default_lease_path(state_dir))
                .release_for_job(job_id)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            let mut finalized = record;
            finalized.phase = OrphanReconciliationPhase::Finalized;
            finalized.finalized_at = Some(Utc::now());
            write_orphan_reconciliation(&record_path, &finalized)?;
            return render_orphan_reconciliation(
                stdout,
                json_mode,
                &job,
                &finalized,
                true,
                "recovered_finalization",
            );
        }
    }

    if job.status != JobStatus::Running || job.cancel_requested_at.is_none() {
        return Err(CliFailure::new(
            1,
            "orphan reconciliation requires a cancel-requested running job",
        ));
    }
    if !job.is_stale_running(
        Utc::now(),
        chrono::Duration::seconds(crate::job::DEFAULT_RUNNING_JOB_STALE_SECONDS),
    ) {
        return Err(CliFailure::new(1, "running job is not stale"));
    }
    let envelope = request_store
        .load(job_id)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .ok_or_else(|| CliFailure::new(1, "durable request is missing"))?;
    if envelope.job_id != job_id
        || envelope.execution_owner != QueuedExecutionOwner::Daemon
        || !envelope.is_daemon_admissible()
    {
        return Err(CliFailure::new(
            1,
            "durable request is not exact daemon-owned work",
        ));
    }
    let provenance = envelope
        .provenance
        .as_ref()
        .ok_or_else(|| CliFailure::new(1, "daemon request provenance is missing"))?;
    if envelope.cwd != provenance.canonical_cwd {
        return Err(CliFailure::new(
            1,
            "daemon request cwd contradicts canonical provenance",
        ));
    }
    let QueuedExecutionRequest::Ship(ship_request) = &envelope.request else {
        return Err(CliFailure::new(
            1,
            "receiptless reconciliation supports ship jobs only",
        ));
    };
    if job.sha != ship_request.sha {
        return Err(CliFailure::new(1, "queue and request heads disagree"));
    }
    if state_dir
        .join("queue-workers")
        .join(format!("{job_id}.json"))
        .exists()
        || state_dir
            .join("queue-terminations")
            .join(format!("{job_id}.json"))
            .exists()
    {
        return Err(CliFailure::new(
            1,
            "worker receipt or termination transaction exists; use normal daemon recovery",
        ));
    }
    let related_processes = inventory_related_processes(job_id, &envelope.cwd)?;
    if !related_processes.is_empty() {
        return Err(CliFailure::new(
            1,
            format!(
                "related processes still exist; refusing reconciliation: {}",
                related_processes.join(" | ")
            ),
        ));
    }

    // The submitted checkout may legitimately have been retired by the time
    // a legacy orphan is reconciled.  Exact repo/PR/head authority comes from
    // the immutable request plus authenticated provider response; only the
    // machine-global auth configuration is needed for that observation.
    let merged = observe_exact_merge(&job, &request_store, global_dir)?
        .filter(|item| {
            item.job_id == job_id
                && item.head_sha == job.sha
                && item.pr == ship_request.pr
                && item.repository == crate::evidence::canonical_repository(&ship_request.repo)
        })
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "authenticated provider did not prove the exact queued head merged",
            )
        })?;
    let proof = CancellationProof {
        cause: crate::job::CancellationCause::AlreadyMerged,
        repository: merged.repository,
        pull_request: merged.pr,
        head_sha: merged.head_sha,
    };
    let record = OrphanReconciliationRecord {
        schema_version: ORPHAN_RECONCILIATION_SCHEMA_VERSION,
        job_id: job_id.to_owned(),
        job_sha256: job_sha256.clone(),
        request_sha256: request_sha256.clone(),
        proof: proof.clone(),
        related_processes,
        prepared_at: Utc::now(),
        phase: OrphanReconciliationPhase::Prepared,
        finalized_at: None,
    };

    if !apply {
        return render_orphan_reconciliation(stdout, json_mode, &job, &record, false, "dry_run");
    }
    let expected_head = expected_head
        .ok_or_else(|| CliFailure::new(2, "--expected-head is required with --apply"))?;
    let expected_request_sha256 = expected_request_sha256
        .ok_or_else(|| CliFailure::new(2, "--expected-request-sha256 is required with --apply"))?;
    let expected_job_sha256 = expected_job_sha256
        .ok_or_else(|| CliFailure::new(2, "--expected-job-sha256 is required with --apply"))?;
    if expected_head != job.sha
        || expected_request_sha256 != request_sha256
        || expected_job_sha256 != job_sha256
    {
        return Err(CliFailure::new(1, "exact apply expectation changed"));
    }
    if !confirm_no_worker_tree {
        return Err(CliFailure::new(
            2,
            "--confirm-no-worker-tree is required with --apply",
        ));
    }
    require_daemon_stopped(state_dir)?;
    let _drain = queue
        .acquire_drain_lock()?
        .ok_or_else(|| CliFailure::new(1, "queue drain lock is owned by another process"))?;
    // Fence the immutable request across the final negative inventory,
    // Prepared receipt, and queue CAS.  The early digest is user-visible
    // review material; this second exact-byte comparison closes the interval
    // in which another writer could otherwise swap repo/PR/head authority.
    let _request_fence = request_store
        .acquire_mutation_lock(job_id)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let fenced_request_bytes = fs::read(&request_path)
        .map_err(|error| CliFailure::new(1, format!("read {}: {error}", request_path.display())))?;
    if fenced_request_bytes != request_bytes
        || hex::encode(Sha256::digest(&fenced_request_bytes)) != request_sha256
    {
        return Err(CliFailure::new(
            1,
            "durable request changed after orphan audit",
        ));
    }
    if !inventory_related_processes(job_id, &envelope.cwd)?.is_empty() {
        return Err(CliFailure::new(
            1,
            "related process inventory changed before apply",
        ));
    }
    write_orphan_reconciliation(&record_path, &record)?;
    let cancelled = queue
        .finalize_audited_receiptless_cancel(
            &exact_queue,
            &job,
            crate::queue::ALREADY_MERGED_CANCEL_REASON.to_owned(),
            proof,
        )?
        .ok_or_else(|| CliFailure::new(1, "queue job disappeared before exact apply"))?;
    persist_terminal_outcome(&cancelled, state_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    HostPoolLeaseStore::new(default_lease_path(state_dir))
        .release_for_job(job_id)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut finalized = record;
    finalized.phase = OrphanReconciliationPhase::Finalized;
    finalized.finalized_at = Some(Utc::now());
    write_orphan_reconciliation(&record_path, &finalized)?;
    render_orphan_reconciliation(stdout, json_mode, &cancelled, &finalized, true, "finalized")
}

fn orphan_record_matches_queue(
    record: &OrphanReconciliationRecord,
    job: &Job,
    job_sha256: &str,
) -> bool {
    let terminal_matches = job.status == JobStatus::Cancelled
        && job.cancellation_proof.as_ref() == Some(&record.proof)
        && job.cancellation_reason.as_deref() == Some(crate::queue::ALREADY_MERGED_CANCEL_REASON);
    match record.phase {
        OrphanReconciliationPhase::Prepared => {
            (job.status == JobStatus::Running && record.job_sha256 == job_sha256)
                || terminal_matches
        }
        OrphanReconciliationPhase::Finalized => terminal_matches,
    }
}

fn canonical_job_sha256(job: &Job) -> Result<String, CliFailure> {
    let bytes = serde_json::to_vec(&job.to_json_value())?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn orphan_reconciliation_path(state_dir: &Path, job_id: &str) -> Result<PathBuf, CliFailure> {
    if !is_plain_component(job_id) {
        return Err(CliFailure::new(2, "job id must be one path component"));
    }
    Ok(state_dir
        .join("queue-reconciliations")
        .join(format!("{job_id}.json")))
}

fn load_orphan_reconciliation(
    path: &Path,
) -> Result<Option<OrphanReconciliationRecord>, CliFailure> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| CliFailure::new(1, error.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CliFailure::new(1, error.to_string())),
    }
}

fn write_orphan_reconciliation(
    path: &Path,
    record: &OrphanReconciliationRecord,
) -> Result<(), CliFailure> {
    let parent = path
        .parent()
        .ok_or_else(|| CliFailure::new(1, "reconciliation path has no parent"))?;
    crate::writer_domain_lease::ensure_protected_dir_all(parent)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let _writer = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    serde_json::to_writer_pretty(&mut temp, record)?;
    temp.write_all(b"\n")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    temp.persist(path)
        .map_err(|error| CliFailure::new(1, error.error.to_string()))?;
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(())
}

fn require_daemon_stopped(state_dir: &Path) -> Result<(), CliFailure> {
    if crate::daemon_ipc::read_daemon_status(state_dir).is_some()
        || state_dir.join("daemon/daemon.pid").exists()
        || state_dir.join("daemon/daemon.sock").exists()
    {
        return Err(CliFailure::new(
            1,
            "daemon must be stopped before applying orphan reconciliation",
        ));
    }
    Ok(())
}

fn related_process_inventory(job_id: &str, cwd: &Path) -> Result<Vec<String>, CliFailure> {
    #[cfg(unix)]
    {
        let mut related = Vec::new();
        let ps = Command::new("/bin/ps")
            .args(["-axo", "pid=,ppid=,command="])
            .output()
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if !ps.status.success() {
            return Err(CliFailure::new(1, "process inventory ps failed"));
        }
        for line in String::from_utf8_lossy(&ps.stdout).lines() {
            if line.contains("execution-worker") && line.contains(job_id) {
                related.push(line.trim().to_owned());
            }
        }
        let lsof = Command::new("/usr/sbin/lsof")
            .args(["-a", "-d", "cwd", "-Fn"])
            .output()
            .map_err(|error| {
                CliFailure::new(1, format!("process cwd inventory failed: {error}"))
            })?;
        if !lsof.status.success() {
            return Err(CliFailure::new(1, "process cwd inventory lsof failed"));
        }
        let canonical = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        if !canonical.is_absolute() {
            return Err(CliFailure::new(1, "durable worker cwd is not absolute"));
        }
        let canonical_text = canonical.to_string_lossy();
        let current_pid = std::process::id().to_string();
        let mut pid = None::<String>;
        for line in String::from_utf8_lossy(&lsof.stdout).lines() {
            if let Some(value) = line.strip_prefix('p') {
                pid = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix('n') {
                let path = Path::new(value);
                // lsof decorates an unlinked working directory as
                // `/exact/path (deleted)`.  Preserve that as related rather
                // than treating a retired checkout as proof of absence.
                let deleted_or_descendant = value
                    .strip_prefix(canonical_text.as_ref())
                    .is_some_and(|suffix| {
                        suffix.is_empty() || suffix.starts_with('/') || suffix.starts_with(' ')
                    });
                if (path == canonical || path.starts_with(&canonical) || deleted_or_descendant)
                    && pid.as_deref() != Some(current_pid.as_str())
                {
                    related.push(format!("pid={} cwd={value}", pid.as_deref().unwrap_or("?")));
                }
            }
        }
        related.sort();
        related.dedup();
        Ok(related)
    }
    #[cfg(not(unix))]
    {
        let _ = (job_id, cwd);
        Err(CliFailure::new(
            1,
            "receiptless orphan process inventory is unsupported on this platform",
        ))
    }
}

fn render_orphan_reconciliation<W: Write>(
    stdout: &mut W,
    json_mode: bool,
    job: &Job,
    record: &OrphanReconciliationRecord,
    applied: bool,
    disposition: &str,
) -> Result<ExitCode, CliFailure> {
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("job_id".to_owned(), Value::String(job.id.clone()));
        data.insert("head_sha".to_owned(), Value::String(job.sha.clone()));
        data.insert(
            "job_sha256".to_owned(),
            Value::String(record.job_sha256.clone()),
        );
        data.insert(
            "request_sha256".to_owned(),
            Value::String(record.request_sha256.clone()),
        );
        data.insert("applied".to_owned(), Value::Bool(applied));
        data.insert(
            "disposition".to_owned(),
            Value::String(disposition.to_owned()),
        );
        data.insert("proof".to_owned(), serde_json::to_value(&record.proof)?);
        data.insert(
            "related_processes".to_owned(),
            serde_json::to_value(&record.related_processes)?,
        );
        write_json_envelope(stdout, "queue-reconcile-orphan", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "{disposition} {} head={} job_sha256={} request_sha256={}",
            job.id, job.sha, record.job_sha256, record.request_sha256
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn bump_command<W: Write>(
    job_id: &str,
    priority: QueuePriority,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut queue = open_queue(state_dir)?;
    let job = queue
        .get(job_id)?
        .ok_or_else(|| CliFailure::new(1, format!("Job {job_id} not found")))?;
    if job.status != JobStatus::Pending {
        return Err(CliFailure::new(
            1,
            format!("Can only bump pending jobs (current: {:?})", job.status),
        ));
    }
    let updated = job.with_priority(priority.into());
    queue.update(&updated)?;
    if json_mode {
        write_job_envelope(stdout, "bump", &updated)?;
    } else {
        writeln!(stdout, "Bumped {job_id} to {}", priority.as_str())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn queue_command<W: Write>(
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut queue = open_queue(state_dir)?;
    let active_runs = queue.get_running()?;
    let active = active_runs.first().cloned();
    let pending = queue.get_pending()?;
    let recent = queue.get_recent(5)?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("active".to_owned(), queue_value(active)?);
        data.insert("active_runs".to_owned(), jobs_value(&active_runs)?);
        data.insert("pending".to_owned(), jobs_value(&pending)?);
        data.insert("recent".to_owned(), jobs_value(&recent)?);
        write_json_envelope(stdout, "queue", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        write_queue_human(stdout, &active_runs, &pending, &recent)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(serde::Serialize)]
struct TargetStatusRow {
    backend: String,
    reachable: bool,
}

fn target_statuses(config: &LoadedConfig) -> Result<BTreeMap<String, TargetStatusRow>, CliFailure> {
    if config.data.get("targets").is_none() {
        return Ok(BTreeMap::new());
    }
    let targets = resolve_targets(config, ValidationMode::Full)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let dispatcher = ExecutorDispatcher::new(None);
    Ok(targets
        .into_iter()
        .map(|target| {
            (
                target.name.clone(),
                TargetStatusRow {
                    backend: target.backend_name.clone(),
                    reachable: dispatcher.probe(&target),
                },
            )
        })
        .collect())
}

fn write_status_human<W: Write>(
    stdout: &mut W,
    running: usize,
    pending: usize,
    recent: &[Job],
    targets: &BTreeMap<String, TargetStatusRow>,
    orphaned: &[(String, u64, crate::ship_liveness::OrphanReport)],
) -> Result<(), CliFailure> {
    writeln!(stdout, "Status").map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "  running: {running}")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "  pending: {pending}")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "  completed_recent: {}", recent.len())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !targets.is_empty() {
        writeln!(stdout, "Targets").map_err(|error| CliFailure::new(1, error.to_string()))?;
        for (name, info) in targets {
            writeln!(
                stdout,
                "  {name}: {} reachable={}",
                info.backend, info.reachable
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    if !orphaned.is_empty() {
        writeln!(
            stdout,
            "Orphaned ship states (in flight, worker likely gone)"
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for (repo, pr, report) in orphaned {
            writeln!(
                stdout,
                "  {repo} PR #{pr}: {} ({}m stalled) — re-run `shipyard ship {pr}` or `ship-state discard {pr}`",
                report.evidence.cause(),
                report.stalled_minutes,
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

fn write_queue_human<W: Write>(
    stdout: &mut W,
    active_runs: &[Job],
    pending: &[Job],
    recent: &[Job],
) -> Result<(), CliFailure> {
    writeln!(stdout, "Queue").map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !active_runs.is_empty() {
        writeln!(stdout, "  Running ({})", active_runs.len())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for active in active_runs {
            writeln!(
                stdout,
                "    {} {} @ {} [{}]",
                active.id,
                active.branch,
                short_sha(&active.sha),
                priority_name(active.priority)
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    if pending.is_empty() {
        writeln!(stdout, "  No pending jobs")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "  Pending ({})", pending.len())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for job in pending {
            writeln!(
                stdout,
                "    {} {} @ {} [{}]",
                job.id,
                job.branch,
                short_sha(&job.sha),
                priority_name(job.priority)
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    if !recent.is_empty() {
        writeln!(stdout, "  Recent ({})", recent.len())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for job in recent {
            writeln!(
                stdout,
                "    {} {} @ {} {}",
                job.id,
                job.branch,
                short_sha(&job.sha),
                if job.passed() { "pass" } else { "fail" }
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

fn open_queue(state_dir: &Path) -> Result<Queue, CliFailure> {
    Queue::new(state_dir).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn queue_value(job: Option<Job>) -> Result<Value, CliFailure> {
    job.map_or(Ok(Value::Null), |job| {
        serde_json::to_value(job.to_json_value())
            .map_err(|error| CliFailure::new(1, error.to_string()))
    })
}

fn jobs_value(jobs: &[Job]) -> Result<Value, CliFailure> {
    serde_json::to_value(jobs.iter().map(Job::to_json_value).collect::<Vec<_>>())
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn write_job_envelope<W: Write>(
    stdout: &mut W,
    command: &str,
    job: &Job,
) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    data.insert("job".to_owned(), job.to_json_value());
    write_json_envelope(stdout, command, data)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn write_log<W: Write>(stdout: &mut W, log_path: &str) -> Result<(), CliFailure> {
    let mut retained = Vec::new();
    for index in (1..=32).rev() {
        let rotated = PathBuf::from(format!("{log_path}.{index}"));
        let compressed = PathBuf::from(format!("{log_path}.{index}.gz"));
        if rotated.exists() {
            retained.push((rotated, false));
        } else if compressed.exists() {
            retained.push((compressed, true));
        }
    }
    let base = PathBuf::from(log_path);
    if base.exists() {
        retained.push((base, false));
    } else {
        let compressed = PathBuf::from(format!("{log_path}.gz"));
        if compressed.exists() {
            retained.push((compressed, true));
        }
    }
    if retained.is_empty() {
        return Err(CliFailure::new(
            1,
            format!("Log file not found: {log_path}"),
        ));
    }
    for (path, compressed) in retained {
        let text = if compressed {
            let file = fs::File::open(&path).map_err(|error| {
                CliFailure::new(1, format!("failed to read {}: {error}", path.display()))
            })?;
            let mut decoder = flate2::read::GzDecoder::new(file);
            let mut text = String::new();
            std::io::Read::read_to_string(&mut decoder, &mut text).map_err(|error| {
                CliFailure::new(1, format!("failed to read {}: {error}", path.display()))
            })?;
            text
        } else {
            fs::read_to_string(&path).map_err(|error| {
                CliFailure::new(1, format!("failed to read {}: {error}", path.display()))
            })?
        };
        write!(stdout, "{text}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn current_git_branch(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

fn priority_name(priority: Priority) -> &'static str {
    match priority {
        Priority::Low => "low",
        Priority::Normal => "normal",
        Priority::High => "high",
    }
}

impl QueuePriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }
}

impl From<QueuePriority> for Priority {
    fn from(value: QueuePriority) -> Self {
        match value {
            QueuePriority::Low => Self::Low,
            QueuePriority::Normal => Self::Normal,
            QueuePriority::High => Self::High,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ORPHAN_RECONCILIATION_SCHEMA_VERSION, OrphanReconciliationPhase,
        OrphanReconciliationRecord, canonical_job_sha256, logs_command, orphan_reconciliation_path,
        orphan_record_matches_queue, reconcile_orphan_command_with, write_log,
        write_orphan_reconciliation,
    };
    use crate::host_pool::{HostPoolLeaseRequest, HostPoolLeaseStore, default_lease_path};
    use crate::job::{
        CancellationCause, CancellationProof, Job, JobKind, JobStatus, Priority, ValidationMode,
    };
    use crate::log_retention::{TERMINAL_MANIFEST_FILE, TerminalLogManifest};
    use crate::queue::Queue;
    use crate::queue_request::{
        ExecutionProvenance, QUEUED_EXECUTION_SCHEMA_VERSION, QueueOutcomeStore, QueueRequestStore,
        QueuedExecutionEnvelope, QueuedExecutionKind, QueuedExecutionOutcome, QueuedExecutionOwner,
        QueuedShipDispositionKind,
    };
    use crate::queue_scheduler::AlreadyMergedCancellation;
    use crate::ship::ShipExecutionRequest;
    use chrono::Utc;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    struct OrphanFixture {
        _temp: tempfile::TempDir,
        global_dir: PathBuf,
        state_dir: PathBuf,
        job: Job,
        pending: Job,
    }

    fn orphan_fixture() -> OrphanFixture {
        let temp = tempfile::tempdir().expect("temp");
        let global_dir = temp.path().join("global");
        let state_dir = temp.path().join("state");
        let cwd = temp.path().join("checkout");
        std::fs::create_dir_all(&cwd).expect("checkout");
        let head = "a".repeat(40);
        let request = ShipExecutionRequest {
            pr: 7863,
            repo: "Generous-Corp/pulp".to_owned(),
            branch: "feature/orphan".to_owned(),
            base_branch: "main".to_owned(),
            sha: head.clone(),
            commit_subject: "orphan fixture".to_owned(),
            pr_url: None,
            pr_title: None,
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: true,
            fail_fast: false,
            resume_from: None,
            advisory_targets: BTreeSet::new(),
            adopt_head: false,
            pr_snapshot_file: None,
            metadata_authority_receipt: None,
            targets: Vec::new(),
        };
        let mut job = Job::create(
            &head,
            &request.branch,
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        )
        .with_kind(JobKind::Ship)
        .start()
        .expect("start")
        .request_cancel_with_reason(Some("operator cancellation".to_owned()))
        .expect("request cancellation");
        job.started_at = Some(Utc::now() - chrono::Duration::minutes(10));
        let mut envelope = QueuedExecutionEnvelope::from_ship_request(&job.id, &cwd, &request);
        envelope.schema_version = QUEUED_EXECUTION_SCHEMA_VERSION;
        envelope.kind = QueuedExecutionKind::Ship;
        envelope.execution_owner = QueuedExecutionOwner::Daemon;
        envelope.provenance = Some(ExecutionProvenance {
            canonical_cwd: cwd.clone(),
            repo_root: cwd.clone(),
            repo_slug: Some(request.repo.clone()),
            head_sha: head.clone(),
            tree_signature: "fixture-tree".to_owned(),
            config_signature: Some("fixture-config".to_owned()),
        });
        QueueRequestStore::new(&state_dir)
            .expect("request store")
            .save(&envelope)
            .expect("request");
        let pending = Job::create(
            "b".repeat(40),
            "feature/pending",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        let mut queue = Queue::new(&state_dir).expect("queue");
        queue.enqueue(job.clone()).expect("orphan");
        queue.enqueue(pending.clone()).expect("pending");
        OrphanFixture {
            _temp: temp,
            global_dir,
            state_dir,
            job,
            pending,
        }
    }

    fn exact_merge(job: &Job) -> AlreadyMergedCancellation {
        AlreadyMergedCancellation {
            job_id: job.id.clone(),
            pr: 7863,
            repository: "generous-corp/pulp".to_owned(),
            head_sha: job.sha.clone(),
        }
    }

    fn acquire_fixture_lease(store: &HostPoolLeaseStore, job: &Job, member_id: &str) {
        store
            .acquire(&HostPoolLeaseRequest {
                pool_name: "mac".to_owned(),
                member_id: member_id.to_owned(),
                target_name: "macos".to_owned(),
                backend: "local".to_owned(),
                host: None,
                job_id: Some(job.id.clone()),
                branch: job.branch.clone(),
                sha: job.sha.clone(),
                max_concurrency: 1,
                lease_stale_seconds: 3_600,
            })
            .expect("lease")
            .expect("acquired");
    }

    fn run_reconciliation(
        fixture: &OrphanFixture,
        expected_head: Option<&str>,
        expected_request_sha256: Option<&str>,
        expected_job_sha256: Option<&str>,
        apply: bool,
        observe: impl FnMut(
            &Job,
            &QueueRequestStore,
            &Path,
        ) -> Result<Option<AlreadyMergedCancellation>, super::CliFailure>,
        inventory: impl FnMut(&str, &Path) -> Result<Vec<String>, super::CliFailure>,
    ) -> Result<serde_json::Value, super::CliFailure> {
        let mut stdout = Vec::new();
        reconcile_orphan_command_with(
            &fixture.job.id,
            expected_head,
            expected_request_sha256,
            expected_job_sha256,
            apply,
            apply,
            &fixture.global_dir,
            &fixture.state_dir,
            true,
            &mut stdout,
            observe,
            inventory,
        )?;
        serde_json::from_slice(&stdout).map_err(super::CliFailure::from)
    }

    fn output_data(value: &serde_json::Value) -> &serde_json::Value {
        value
    }

    #[test]
    fn reconciliation_phase_state_matrix_fails_closed() {
        let running = Job::create(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "feature/test",
            vec!["mac".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        )
        .start()
        .expect("start")
        .request_cancel_with_reason(Some("operator request".to_owned()))
        .expect("cancel request");
        let digest = canonical_job_sha256(&running).expect("digest");
        let proof = CancellationProof {
            cause: CancellationCause::AlreadyMerged,
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            head_sha: running.sha.clone(),
        };
        let prepared = OrphanReconciliationRecord {
            schema_version: ORPHAN_RECONCILIATION_SCHEMA_VERSION,
            job_id: running.id.clone(),
            job_sha256: digest.clone(),
            request_sha256: "b".repeat(64),
            proof: proof.clone(),
            related_processes: Vec::new(),
            prepared_at: Utc::now(),
            phase: OrphanReconciliationPhase::Prepared,
            finalized_at: None,
        };
        assert!(orphan_record_matches_queue(&prepared, &running, &digest));

        let cancelled = running
            .cancel_with_reason_and_proof(
                Some(crate::queue::ALREADY_MERGED_CANCEL_REASON.to_owned()),
                Some(proof),
            )
            .expect("terminal cancel");
        assert!(orphan_record_matches_queue(&prepared, &cancelled, &digest));
        let mut finalized = prepared;
        finalized.phase = OrphanReconciliationPhase::Finalized;
        finalized.finalized_at = Some(Utc::now());
        assert!(!orphan_record_matches_queue(&finalized, &running, &digest));
        assert!(orphan_record_matches_queue(&finalized, &cancelled, &digest));
    }

    #[test]
    fn orphan_reconciliation_dry_run_then_apply_publishes_outcome_and_releases_only_its_lease() {
        let fixture = orphan_fixture();
        let leases = HostPoolLeaseStore::new(default_lease_path(&fixture.state_dir));
        for (job, member) in [
            (&fixture.job, "orphan-host"),
            (&fixture.pending, "pending-host"),
        ] {
            acquire_fixture_lease(&leases, job, member);
        }

        let dry_run = run_reconciliation(
            &fixture,
            None,
            None,
            None,
            false,
            |job, _, _| Ok(Some(exact_merge(job))),
            |_, _| Ok(Vec::new()),
        )
        .expect("dry run");
        let data = output_data(&dry_run);
        assert_eq!(data["disposition"], "dry_run");
        assert_eq!(data["applied"], false);
        assert_eq!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get(&fixture.job.id)
                .expect("read")
                .expect("orphan")
                .status,
            JobStatus::Running
        );

        let applied = run_reconciliation(
            &fixture,
            Some(&fixture.job.sha),
            data["request_sha256"].as_str(),
            data["job_sha256"].as_str(),
            true,
            |job, _, _| Ok(Some(exact_merge(job))),
            |_, _| Ok(Vec::new()),
        )
        .expect("apply");
        assert_eq!(output_data(&applied)["disposition"], "finalized");
        assert_eq!(output_data(&applied)["applied"], true);

        let mut queue = Queue::new(&fixture.state_dir).expect("queue");
        let terminal = queue
            .get(&fixture.job.id)
            .expect("read")
            .expect("terminal orphan");
        assert_eq!(terminal.status, JobStatus::Cancelled);
        assert_eq!(
            terminal.cancellation_reason.as_deref(),
            Some(crate::queue::ALREADY_MERGED_CANCEL_REASON)
        );
        assert_eq!(
            terminal.cancellation_proof,
            Some(CancellationProof {
                cause: CancellationCause::AlreadyMerged,
                repository: "generous-corp/pulp".to_owned(),
                pull_request: 7863,
                head_sha: fixture.job.sha.clone(),
            })
        );
        assert_eq!(
            queue
                .get(&fixture.pending.id)
                .expect("read pending")
                .expect("pending job"),
            fixture.pending
        );

        let outcome = QueueOutcomeStore::new(&fixture.state_dir)
            .expect("outcome store")
            .load(&fixture.job.id)
            .expect("load outcome")
            .expect("published outcome");
        assert!(matches!(
            outcome,
            QueuedExecutionOutcome::Ship {
                post_validation: Some(ref disposition),
                ..
            } if disposition.kind == QueuedShipDispositionKind::AlreadyMerged
                && disposition.exit_code == 0
        ));
        let remaining = leases.leases().expect("leases");
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].job_id.as_deref(),
            Some(fixture.pending.id.as_str())
        );
        let record = std::fs::read_to_string(
            orphan_reconciliation_path(&fixture.state_dir, &fixture.job.id).expect("record path"),
        )
        .expect("record");
        assert_eq!(
            serde_json::from_str::<OrphanReconciliationRecord>(&record)
                .expect("record JSON")
                .phase,
            OrphanReconciliationPhase::Finalized
        );
    }

    #[test]
    fn orphan_reconciliation_refuses_related_process_before_merge_observation() {
        let fixture = orphan_fixture();
        let mut observed = false;
        let error = run_reconciliation(
            &fixture,
            None,
            None,
            None,
            false,
            |job, _, _| {
                observed = true;
                Ok(Some(exact_merge(job)))
            },
            |job_id, cwd| Ok(vec![format!("pid=4242 job={job_id} cwd={}", cwd.display())]),
        )
        .expect_err("live process must refuse");
        assert!(error.message.contains("related processes still exist"));
        assert!(
            !observed,
            "provider observation must not run past process refusal"
        );
    }

    #[test]
    fn orphan_reconciliation_refuses_process_appearing_at_final_apply_inventory() {
        let fixture = orphan_fixture();
        let dry_run = run_reconciliation(
            &fixture,
            None,
            None,
            None,
            false,
            |job, _, _| Ok(Some(exact_merge(job))),
            |_, _| Ok(Vec::new()),
        )
        .expect("dry run");
        let data = output_data(&dry_run);
        let mut inventories = 0;
        let error = run_reconciliation(
            &fixture,
            Some(&fixture.job.sha),
            data["request_sha256"].as_str(),
            data["job_sha256"].as_str(),
            true,
            |job, _, _| Ok(Some(exact_merge(job))),
            |_, _| {
                inventories += 1;
                Ok(if inventories == 2 {
                    vec!["pid=4242 cwd=/fixture".to_owned()]
                } else {
                    Vec::new()
                })
            },
        )
        .expect_err("process appearing before queue CAS must refuse");
        assert!(
            error
                .message
                .contains("related process inventory changed before apply")
        );
        assert_eq!(inventories, 2);
        assert_eq!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get(&fixture.job.id)
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running
        );
    }

    #[test]
    fn orphan_reconciliation_refuses_daemon_on_apply_without_mutating_queue() {
        let fixture = orphan_fixture();
        let dry_run = run_reconciliation(
            &fixture,
            None,
            None,
            None,
            false,
            |job, _, _| Ok(Some(exact_merge(job))),
            |_, _| Ok(Vec::new()),
        )
        .expect("dry run");
        let data = output_data(&dry_run);
        let daemon = fixture.state_dir.join("daemon");
        std::fs::create_dir_all(&daemon).expect("daemon dir");
        std::fs::write(daemon.join("daemon.pid"), "4242\n").expect("daemon marker");
        let error = run_reconciliation(
            &fixture,
            Some(&fixture.job.sha),
            data["request_sha256"].as_str(),
            data["job_sha256"].as_str(),
            true,
            |job, _, _| Ok(Some(exact_merge(job))),
            |_, _| Ok(Vec::new()),
        )
        .expect_err("running daemon must refuse apply");
        assert!(error.message.contains("daemon must be stopped"));
        assert_eq!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get(&fixture.job.id)
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running
        );
        assert!(
            !orphan_reconciliation_path(&fixture.state_dir, &fixture.job.id)
                .expect("record")
                .exists()
        );
    }

    #[test]
    fn orphan_reconciliation_refuses_nonexact_merged_head() {
        let fixture = orphan_fixture();
        let error = run_reconciliation(
            &fixture,
            None,
            None,
            None,
            false,
            |job, _, _| {
                let mut mismatch = exact_merge(job);
                mismatch.head_sha = "f".repeat(40);
                Ok(Some(mismatch))
            },
            |_, _| Ok(Vec::new()),
        )
        .expect_err("different merged head must refuse");
        assert!(
            error
                .message
                .contains("did not prove the exact queued head merged")
        );
        assert_eq!(
            Queue::new(&fixture.state_dir)
                .expect("queue")
                .get(&fixture.job.id)
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running
        );
    }

    #[test]
    fn prepared_reconciliation_recovers_outcome_and_lease_after_queue_terminalization() {
        let fixture = orphan_fixture();
        let request_path = QueueRequestStore::new(&fixture.state_dir)
            .expect("request store")
            .path_for(&fixture.job.id);
        let request_sha256 = hex::encode(Sha256::digest(
            std::fs::read(request_path).expect("request bytes"),
        ));
        let job_sha256 = canonical_job_sha256(&fixture.job).expect("job digest");
        let proof = CancellationProof {
            cause: CancellationCause::AlreadyMerged,
            repository: "generous-corp/pulp".to_owned(),
            pull_request: 7863,
            head_sha: fixture.job.sha.clone(),
        };
        let record = OrphanReconciliationRecord {
            schema_version: ORPHAN_RECONCILIATION_SCHEMA_VERSION,
            job_id: fixture.job.id.clone(),
            job_sha256,
            request_sha256,
            proof: proof.clone(),
            related_processes: Vec::new(),
            prepared_at: Utc::now(),
            phase: OrphanReconciliationPhase::Prepared,
            finalized_at: None,
        };
        write_orphan_reconciliation(
            &orphan_reconciliation_path(&fixture.state_dir, &fixture.job.id).expect("record path"),
            &record,
        )
        .expect("prepared record");
        let leases = HostPoolLeaseStore::new(default_lease_path(&fixture.state_dir));
        acquire_fixture_lease(&leases, &fixture.job, "orphan-host");
        let mut queue = Queue::new(&fixture.state_dir).expect("queue");
        let exact_queue = queue.get_all().expect("snapshot");
        queue
            .finalize_audited_receiptless_cancel(
                &exact_queue,
                &fixture.job,
                crate::queue::ALREADY_MERGED_CANCEL_REASON.to_owned(),
                proof,
            )
            .expect("terminalize")
            .expect("terminal job");

        let recovered = run_reconciliation(
            &fixture,
            None,
            None,
            None,
            false,
            |_, _, _| panic!("prepared recovery must not re-query provider"),
            |_, _| panic!("prepared recovery must not repeat process inventory"),
        )
        .expect("recover finalization");
        assert_eq!(
            output_data(&recovered)["disposition"],
            "recovered_finalization"
        );
        assert_eq!(output_data(&recovered)["applied"], true);
        assert!(leases.leases().expect("leases").is_empty());
        assert!(
            QueueOutcomeStore::new(&fixture.state_dir)
                .expect("outcome store")
                .load(&fixture.job.id)
                .expect("outcome")
                .is_some()
        );
        let finalized: OrphanReconciliationRecord = serde_json::from_slice(
            &std::fs::read(
                orphan_reconciliation_path(&fixture.state_dir, &fixture.job.id)
                    .expect("record path"),
            )
            .expect("record"),
        )
        .expect("record JSON");
        assert_eq!(finalized.phase, OrphanReconciliationPhase::Finalized);
        assert!(finalized.finalized_at.is_some());
    }

    #[test]
    fn write_log_falls_back_to_retained_gzip() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("target.log");
        let output = std::fs::File::create(format!("{}.gz", path.display())).expect("gzip");
        let mut encoder = GzEncoder::new(output, Compression::fast());
        encoder.write_all(b"retained evidence\n").expect("write");
        encoder.finish().expect("finish");
        let mut stdout = Vec::new();
        write_log(&mut stdout, path.to_str().expect("path")).expect("read gzip");
        assert_eq!(stdout, b"retained evidence\n");
    }

    #[test]
    fn write_log_includes_rotated_segments_oldest_first() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("target.log");
        std::fs::write(format!("{}.2", path.display()), "oldest\n").expect("oldest");
        std::fs::write(format!("{}.1", path.display()), "older\n").expect("older");
        std::fs::write(&path, "active\n").expect("active");
        let mut stdout = Vec::new();
        write_log(&mut stdout, path.to_str().expect("path")).expect("read history");
        assert_eq!(stdout, b"oldest\nolder\nactive\n");
    }

    #[test]
    fn trimmed_terminal_job_log_is_readable_by_target() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join("queue.json"), r#"{"jobs":[]}"#).expect("queue");
        let job_dir = temp.path().join("logs/job");
        std::fs::create_dir_all(&job_dir).expect("job dir");
        std::fs::write(job_dir.join("macos.log"), "first attempt\n").expect("log");
        std::fs::write(job_dir.join("macos.log.attempt-1"), "first failover\n").expect("failover");
        std::fs::write(job_dir.join("macos.log.retry1"), "terminal retry\n").expect("retry");
        let nested = std::fs::File::create(job_dir.join("macos.log.retry1.attempt-2.gz"))
            .expect("nested gzip");
        let mut encoder = GzEncoder::new(nested, Compression::fast());
        encoder
            .write_all(b"terminal failover\n")
            .expect("nested log");
        encoder.finish().expect("finish nested gzip");
        let manifest = TerminalLogManifest {
            schema_version: 1,
            job_id: "job".to_owned(),
            terminal_at: Utc::now(),
            failed: false,
            reason: "passed".to_owned(),
        };
        std::fs::write(
            job_dir.join(TERMINAL_MANIFEST_FILE),
            serde_json::to_vec(&manifest).expect("manifest"),
        )
        .expect("manifest");
        let mut stdout = Vec::new();
        logs_command("job", Some("macos".to_owned()), temp.path(), &mut stdout)
            .expect("retained log");
        assert_eq!(
            stdout,
            b"first attempt\nfirst failover\nterminal retry\nterminal failover\n"
        );

        std::fs::remove_file(job_dir.join(TERMINAL_MANIFEST_FILE)).expect("remove manifest");
        let mut unclassified = Vec::new();
        logs_command(
            "job",
            Some("macos".to_owned()),
            temp.path(),
            &mut unclassified,
        )
        .expect("unclassified retained log");
        assert_eq!(unclassified, stdout);

        let error = logs_command(
            "../job",
            Some("macos".to_owned()),
            temp.path(),
            &mut Vec::new(),
        )
        .expect_err("job traversal rejected");
        assert_eq!(error.code, 2);
    }
}
