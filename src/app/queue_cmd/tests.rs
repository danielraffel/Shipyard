use super::orphan_reconciliation::{
    ORPHAN_RECONCILIATION_SCHEMA_VERSION, OrphanReconciliationPhase, OrphanReconciliationRecord,
    canonical_job_sha256, orphan_reconciliation_path, orphan_record_matches_queue,
    reconcile_orphan_command_with, write_orphan_reconciliation,
};
use super::{logs_command, write_log};
use crate::host_pool::{HostPoolLeaseRequest, HostPoolLeaseStore, default_lease_path};
use crate::job::{
    CancellationCause, CancellationProof, Job, JobKind, JobStatus, Priority, TargetResult,
    TargetStatus, ValidationMode,
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
            orphan_reconciliation_path(&fixture.state_dir, &fixture.job.id).expect("record path"),
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
    let nested =
        std::fs::File::create(job_dir.join("macos.log.retry1.attempt-2.gz")).expect("nested gzip");
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
    let mut lifecycle = Vec::new();
    assert_eq!(
        logs_command("job", None, temp.path(), true, &mut lifecycle).expect("lifecycle"),
        std::process::ExitCode::SUCCESS
    );
    let lifecycle: serde_json::Value = serde_json::from_slice(&lifecycle).expect("json");
    assert_eq!(lifecycle["job_status"], "completed");
    assert_eq!(lifecycle["terminal"], true);
    assert_eq!(lifecycle["passed"], true);
    assert_eq!(lifecycle["observation"], "retained");

    let mut target_state = Vec::new();
    logs_command("job", Some("macos"), temp.path(), true, &mut target_state)
        .expect("retained target state");
    let target_state: serde_json::Value = serde_json::from_slice(&target_state).expect("json");
    assert_eq!(target_state["requested_target"], "macos");
    assert_eq!(target_state["observation"], "retained");
    let mut absent = Vec::new();
    assert_eq!(
        logs_command("job", Some("windows"), temp.path(), true, &mut absent)
            .expect("absent retained target"),
        std::process::ExitCode::from(1)
    );
    let absent: serde_json::Value = serde_json::from_slice(&absent).expect("json");
    assert_eq!(absent["observation"], "not_materialized");

    let mut stdout = Vec::new();
    logs_command("job", Some("macos"), temp.path(), false, &mut stdout).expect("retained log");
    assert_eq!(
        stdout,
        b"first attempt\nfirst failover\nterminal retry\nterminal failover\n"
    );

    std::fs::remove_file(job_dir.join(TERMINAL_MANIFEST_FILE)).expect("remove manifest");
    let mut unclassified = Vec::new();
    logs_command("job", Some("macos"), temp.path(), false, &mut unclassified)
        .expect("unclassified retained log");
    assert_eq!(unclassified, stdout);

    let error = logs_command("../job", Some("macos"), temp.path(), false, &mut Vec::new())
        .expect_err("job traversal rejected");
    assert_eq!(error.code, 2);
}

#[test]
fn logs_json_keeps_pending_without_materialized_logs_nonterminal() {
    let temp = tempfile::tempdir().expect("temp");
    let mut queue = Queue::new(temp.path()).expect("queue");
    let job = queue
        .enqueue(Job::create(
            "pending-head",
            "feature/pending-logs",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        ))
        .expect("enqueue");
    let mut stdout = Vec::new();

    let code = logs_command(&job.id, None, temp.path(), true, &mut stdout).expect("logs json");
    let payload: serde_json::Value = serde_json::from_slice(&stdout).expect("json");

    assert_eq!(code, std::process::ExitCode::from(3));
    assert_eq!(payload["command"], "logs");
    assert_eq!(payload["job_status"], "pending");
    assert_eq!(payload["observation"], "not_materialized");
    assert_eq!(payload["terminal"], false);
    assert!(payload["passed"].is_null());
    assert_eq!(payload["available_targets"], serde_json::json!([]));
}

#[test]
fn logs_json_keeps_manifest_only_terminal_job_not_materialized() {
    let temp = tempfile::tempdir().expect("temp");
    let mut queue = Queue::new(temp.path()).expect("queue");
    let pending = queue
        .enqueue(Job::create(
            "cancelled-head",
            "feature/no-logs",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        ))
        .expect("enqueue");
    let cancelled = pending.cancel().expect("cancel");
    queue.update(&cancelled).expect("update");
    let mut stdout = Vec::new();

    let code =
        logs_command(&cancelled.id, None, temp.path(), true, &mut stdout).expect("terminal json");
    let payload: serde_json::Value = serde_json::from_slice(&stdout).expect("json");

    assert_eq!(code, std::process::ExitCode::from(1));
    assert_eq!(payload["observation"], "not_materialized");
    assert_eq!(payload["terminal"], true);
    assert_eq!(payload["passed"], false);
}

#[test]
fn logs_json_reports_available_running_log_without_printing_or_implying_success() {
    let temp = tempfile::tempdir().expect("temp");
    let log_path = temp.path().join("macos.log");
    std::fs::write(&log_path, "secret-ish raw log content\n").expect("log");
    let mut queue = Queue::new(temp.path()).expect("queue");
    let pending = queue
        .enqueue(Job::create(
            "running-head",
            "feature/running-logs",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        ))
        .expect("enqueue");
    let mut result = TargetResult::new("macos", "macos", TargetStatus::Running, "local");
    result.log_path = Some(log_path.to_string_lossy().into_owned());
    let running = pending.start().expect("start").with_result(result);
    queue.update(&running).expect("running");
    let mut stdout = Vec::new();

    let code = logs_command(&running.id, Some("macos"), temp.path(), true, &mut stdout)
        .expect("logs json");
    let payload: serde_json::Value = serde_json::from_slice(&stdout).expect("json");

    assert_eq!(code, std::process::ExitCode::from(3));
    assert_eq!(payload["job_status"], "running");
    assert_eq!(payload["observation"], "available");
    assert_eq!(payload["terminal"], false);
    assert!(payload["passed"].is_null());
    assert_eq!(payload["available_targets"], serde_json::json!(["macos"]));
    assert!(
        !String::from_utf8(stdout)
            .expect("utf8")
            .contains("raw log content")
    );

    std::fs::remove_file(log_path).expect("remove log");
    let mut missing = Vec::new();
    logs_command(&running.id, Some("macos"), temp.path(), true, &mut missing)
        .expect("missing log state");
    let missing: serde_json::Value = serde_json::from_slice(&missing).expect("json");
    assert_eq!(missing["observation"], "not_materialized");
    assert_eq!(missing["available_targets"], serde_json::json!([]));
}

#[test]
fn logs_json_distinguishes_terminal_without_log_missing_job_and_invalid_target() {
    let temp = tempfile::tempdir().expect("temp");
    let mut queue = Queue::new(temp.path()).expect("queue");
    let pending = queue
        .enqueue(Job::create(
            "terminal-head",
            "feature/terminal-logs",
            vec!["linux".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        ))
        .expect("enqueue");
    let running = pending
        .start()
        .expect("start")
        .with_result(TargetResult::new(
            "linux",
            "linux",
            TargetStatus::Pass,
            "local",
        ));
    queue.update(&running).expect("running");
    let completed = running.complete().expect("complete");
    queue.update(&completed).expect("complete");

    let mut terminal_out = Vec::new();
    assert_eq!(
        logs_command(&completed.id, None, temp.path(), true, &mut terminal_out)
            .expect("terminal json"),
        std::process::ExitCode::from(1)
    );
    let terminal: serde_json::Value = serde_json::from_slice(&terminal_out).expect("terminal json");
    assert_eq!(terminal["terminal"], true);
    assert_eq!(terminal["passed"], true);
    assert_eq!(terminal["observation"], "not_materialized");

    let mut missing_out = Vec::new();
    assert_eq!(
        logs_command("sy-missing", None, temp.path(), true, &mut missing_out)
            .expect("missing json"),
        std::process::ExitCode::from(5)
    );
    let missing: serde_json::Value = serde_json::from_slice(&missing_out).expect("missing json");
    assert!(missing["job_status"].is_null());
    assert!(missing["terminal"].is_null());
    assert!(missing["passed"].is_null());
    assert_eq!(missing["observation"], "not_found");

    let mut invalid_out = Vec::new();
    assert_eq!(
        logs_command(
            &completed.id,
            Some("windows"),
            temp.path(),
            true,
            &mut invalid_out,
        )
        .expect("invalid target json"),
        std::process::ExitCode::from(2)
    );
    let invalid: serde_json::Value = serde_json::from_slice(&invalid_out).expect("invalid json");
    assert_eq!(invalid["observation"], "invalid_target");
    assert_eq!(invalid["terminal"], true);
}
