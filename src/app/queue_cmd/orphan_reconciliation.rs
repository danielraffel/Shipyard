use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
use std::process::ExitCode;

use chrono::Utc;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{CliFailure, is_plain_component, open_queue};
use crate::config::LoadedConfig;
use crate::host_pool::{HostPoolLeaseStore, default_lease_path};
use crate::job::{CancellationProof, Job, JobStatus};
use crate::output::write_json_envelope;
use crate::queue_request::{QueueRequestStore, QueuedExecutionOwner, QueuedExecutionRequest};
use crate::queue_scheduler::AlreadyMergedObserver;
use crate::ship::persist_terminal_outcome;

pub(super) const ORPHAN_RECONCILIATION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OrphanReconciliationPhase {
    Prepared,
    Finalized,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct OrphanReconciliationRecord {
    pub(super) schema_version: u32,
    pub(super) job_id: String,
    pub(super) job_sha256: String,
    pub(super) request_sha256: String,
    pub(super) proof: CancellationProof,
    pub(super) related_processes: Vec<String>,
    pub(super) prepared_at: chrono::DateTime<Utc>,
    pub(super) phase: OrphanReconciliationPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) finalized_at: Option<chrono::DateTime<Utc>>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn reconcile_orphan_command<W: Write>(
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
pub(super) fn reconcile_orphan_command_with<W, O, I>(
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

pub(super) fn orphan_record_matches_queue(
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

pub(super) fn canonical_job_sha256(job: &Job) -> Result<String, CliFailure> {
    let bytes = serde_json::to_vec(&job.to_json_value())?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub(super) fn orphan_reconciliation_path(
    state_dir: &Path,
    job_id: &str,
) -> Result<PathBuf, CliFailure> {
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

pub(super) fn write_orphan_reconciliation(
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
    crate::log_retention::sync_parent_directory(path)
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
