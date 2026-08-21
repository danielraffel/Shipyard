use super::witness::remove_recovery_witness;
use super::*;
use crate::config::LocalOverlaySource;
#[cfg(unix)]
use crate::recovery_worker::RecoveryOutput;

fn required_check(context: &str) -> RecoveryFailureFact {
    RecoveryFailureFact::RequiredCheck {
        context: context.to_owned(),
        app_id: None,
        conclusion: "FAILURE".to_owned(),
        run_id: None,
    }
}

fn required_policy(context: &str) -> Vec<RecoveryRequiredCheck> {
    vec![RecoveryRequiredCheck {
        context: context.to_owned(),
        app_id: None,
    }]
}

fn config(contents: &str) -> LoadedConfig {
    LoadedConfig {
        data: contents.parse().expect("valid TOML fixture"),
        global_dir: PathBuf::from("/trusted"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: LocalOverlaySource::None,
    }
}

fn valid_policy() -> LoadedConfig {
    config(&recovery_test_policy_toml(&recovery_test_repo_path()))
}

fn policy_with_repo_path(repo_path: &Path) -> LoadedConfig {
    config(&recovery_test_policy_toml(repo_path))
}

#[test]
fn preclaim_repository_error_durably_rotates_to_the_next_request() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path().join("store")).expect("store");
    drop(
        acquire_recovery_enqueue_lease(store.root(), recovery_lease_deadline())
            .expect("enqueue lease fixture"),
    );
    let trusted_config = policy_with_repo_path(&temp.path().join("missing-repository"));
    let policy = RecoveryWorkerPolicy::from_config(&trusted_config).expect("policy");
    let signature = policy.signature().expect("signature");
    let mut first = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-one",
        "first required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        &signature,
    )
    .expect("first request");
    let mut second = RecoveryRequest::new(
        "Generous-Corp/pulp",
        43,
        "main",
        "fedcba9876543210fedcba9876543210fedcba98",
        "failure-two",
        "second required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        &signature,
    )
    .expect("second request");
    first.created_at = chrono::Utc::now() - chrono::Duration::seconds(2);
    second.created_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    write_recovery_witness(
        temp.path(),
        &first.repo,
        first.pr,
        &first.id,
        &first.head_sha,
        &first.policy_signature,
        &first.failure_fingerprint,
    )
    .expect("first witness");
    store.enqueue(first.clone()).expect("first enqueue");
    store.enqueue(second.clone()).expect("second enqueue");
    let record = store.get(&first.id).expect("load").expect("first record");

    let error = process_record(ProcessRecordInputs {
        store: &store,
        record: &record,
        apply: true,
        policy: &policy,
        policy_signature: &signature,
        trusted_config: &trusted_config,
        model_lease: None,
        state_dir: temp.path(),
        scratch_dir: &temp.path().join("scratch"),
    })
    .expect_err("missing repository fails preflight");
    assert!(error.message().contains("is not a directory"));
    let deferred = store.get(&first.id).expect("load").expect("deferred");
    assert_eq!(deferred.receipt.status, RecoveryStatus::Pending);
    assert_eq!(deferred.receipt.attempt, 0);
    assert!(deferred.receipt.deferred_at.is_some());
    assert_eq!(
        store.pending(1).expect("next pending")[0].request.id,
        second.id
    );
}

#[test]
fn pending_config_drift_is_superseded_without_launching() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path().join("store")).expect("store");
    drop(
        acquire_recovery_enqueue_lease(store.root(), recovery_lease_deadline())
            .expect("enqueue lease fixture"),
    );
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "old-worker-config",
    )
    .expect("request");
    let id = request.id.clone();
    write_recovery_witness(
        temp.path(),
        &request.repo,
        request.pr,
        &request.id,
        &request.head_sha,
        &request.policy_signature,
        &request.failure_fingerprint,
    )
    .expect("witness");
    store.enqueue(request).expect("enqueue");
    let record = store.get(&id).expect("load").expect("durable record");
    let trusted_config = valid_policy();
    let policy = RecoveryWorkerPolicy::from_config(&trusted_config).expect("policy");
    let current_signature = policy.signature().expect("signature");

    let dry_run = process_record(ProcessRecordInputs {
        store: &store,
        record: &record,
        apply: false,
        policy: &policy,
        policy_signature: &current_signature,
        trusted_config: &trusted_config,
        model_lease: None,
        state_dir: temp.path(),
        scratch_dir: &temp.path().join("scratch"),
    })
    .expect("dry-run drift");
    assert_eq!(dry_run.action, "would_supersede");
    assert_eq!(
        store
            .get(&id)
            .expect("load")
            .expect("record")
            .receipt
            .status,
        crate::recovery_worker::RecoveryStatus::Pending
    );

    let applied = process_record(ProcessRecordInputs {
        store: &store,
        record: &record,
        apply: true,
        policy: &policy,
        policy_signature: &current_signature,
        trusted_config: &trusted_config,
        model_lease: None,
        state_dir: temp.path(),
        scratch_dir: &temp.path().join("scratch"),
    })
    .expect("apply drift");
    assert_eq!(applied.action, "superseded");
    assert_eq!(
        store
            .get(&id)
            .expect("load")
            .expect("record")
            .receipt
            .status,
        crate::recovery_worker::RecoveryStatus::Superseded
    );
}

#[test]
fn stale_pending_snapshot_cannot_supersede_reactivated_configuration() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path().join("store")).expect("store");
    drop(
        acquire_recovery_enqueue_lease(store.root(), recovery_lease_deadline())
            .expect("enqueue lease fixture"),
    );
    let stale_request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "old-worker-config",
    )
    .expect("stale request");
    let id = stale_request.id.clone();
    store.enqueue(stale_request).expect("enqueue stale request");
    let stale_record = store.get(&id).expect("load").expect("stale snapshot");

    let trusted_config = valid_policy();
    let policy = RecoveryWorkerPolicy::from_config(&trusted_config).expect("policy");
    let current_signature = policy.signature().expect("signature");
    let current_request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        &current_signature,
    )
    .expect("current request");
    assert_eq!(current_request.id, id);
    store
        .enqueue(current_request)
        .expect("reactivate with current configuration");

    let report = process_record(ProcessRecordInputs {
        store: &store,
        record: &stale_record,
        apply: true,
        policy: &policy,
        policy_signature: &current_signature,
        trusted_config: &trusted_config,
        model_lease: None,
        state_dir: temp.path(),
        scratch_dir: &temp.path().join("scratch"),
    })
    .expect("stale snapshot is ignored");
    assert_eq!(report.action, "stale_snapshot");
    let durable = store.get(&id).expect("load").expect("durable record");
    assert_eq!(
        durable.receipt.status,
        crate::recovery_worker::RecoveryStatus::Pending
    );
    assert_eq!(durable.request.config_signature, current_signature);
}

#[test]
fn bounded_tail_keeps_only_final_bytes() {
    assert_eq!(
        process::read_bounded_tail(&b"0123456789"[..], 4)
            .expect("tail")
            .tail,
        b"6789"
    );
}

#[test]
fn enqueue_lease_serializes_witness_and_completion_sections() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store_root = temp.path().join("store");
    fs::create_dir_all(&store_root).expect("store root");
    let first = acquire_recovery_enqueue_lease(&store_root, recovery_lease_deadline())
        .expect("first lease");
    let (sender, receiver) = std::sync::mpsc::channel();
    let competing_root = store_root.clone();
    let competitor = std::thread::spawn(move || {
        let lease = acquire_recovery_enqueue_lease(
            &competing_root,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("competing lease");
        sender.send(()).expect("send acquired signal");
        drop(lease);
    });

    assert!(
        receiver.recv_timeout(Duration::from_millis(100)).is_err(),
        "competing witness/completion section must block"
    );
    drop(first);
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("competing section proceeds after release");
    competitor.join().expect("competitor");
}

#[test]
fn enqueue_lease_serializes_initial_witness_read_without_creating_a_lock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store_root = temp.path().join("store");
    fs::create_dir_all(&store_root).expect("store root");
    assert!(acquire_recovery_enqueue_read_lease(&store_root, recovery_lease_deadline()).is_err());
    assert!(!store_root.join("enqueue-witness.lock").exists());

    let exclusive = acquire_recovery_enqueue_lease(&store_root, recovery_lease_deadline())
        .expect("exclusive lease");
    let (sender, receiver) = std::sync::mpsc::channel();
    let competing_root = store_root.clone();
    let reader = std::thread::spawn(move || {
        let lease = acquire_recovery_enqueue_read_lease(
            &competing_root,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("shared read lease");
        sender.send(()).expect("send acquired signal");
        drop(lease);
    });
    assert!(
        receiver.recv_timeout(Duration::from_millis(100)).is_err(),
        "initial witness read must wait for enqueue publication"
    );
    drop(exclusive);
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("witness read proceeds after enqueue publication");
    reader.join().expect("reader");
}

#[test]
fn global_model_lease_path_ignores_cli_state_and_runtime_mode_overrides() {
    let overridden = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(PathBuf::from("/tmp/recovery-global-override")),
        Some(PathBuf::from("/tmp/recovery-state-override")),
    );
    let canonical = canonical_global_model_lease_path().expect("stable account path");
    let canonical_paths = canonical_recovery_paths().expect("stable account paths");
    assert_eq!(
        canonical,
        canonical_paths
            .state_dir
            .join("merge-steward/recovery/global-model.lock")
    );
    assert_ne!(canonical, overridden.state_dir.join("global-model.lock"));
    let normal_runtime_paths = RuntimePaths::current(RuntimeMode::Shipyard);
    assert_eq!(normal_runtime_paths.global_dir, canonical_paths.global_dir);
    assert_eq!(normal_runtime_paths.state_dir, canonical_paths.state_dir);
    ensure_canonical_recovery_paths(&canonical_paths.global_dir, &canonical_paths.state_dir)
        .expect("canonical paths accepted");
    let error = ensure_canonical_recovery_paths(&overridden.global_dir, &overridden.state_dir)
        .expect_err("alternate attempt ledger rejected");
    assert!(
        error
            .message()
            .contains("cannot fork policy or attempt accounting")
    );
}

#[cfg(any(unix, windows))]
#[test]
fn canonical_recovery_authority_ignores_home_and_working_directory() {
    const PROBE_ENV: &str = "SHIPYARD_TEST_CANONICAL_RECOVERY_AUTHORITY_PROBE";
    const EXPECTED_GLOBAL_ENV: &str = "SHIPYARD_TEST_EXPECTED_RECOVERY_GLOBAL";
    const EXPECTED_STATE_ENV: &str = "SHIPYARD_TEST_EXPECTED_RECOVERY_STATE";

    if std::env::var_os(PROBE_ENV).is_some() {
        let canonical = canonical_recovery_paths().expect("stable account paths");
        assert!(canonical.global_dir.is_absolute());
        assert!(canonical.state_dir.is_absolute());
        assert_eq!(
            canonical.global_dir,
            PathBuf::from(
                std::env::var_os(EXPECTED_GLOBAL_ENV).expect("expected global path fixture")
            )
        );
        assert_eq!(
            canonical.state_dir,
            PathBuf::from(
                std::env::var_os(EXPECTED_STATE_ENV).expect("expected state path fixture")
            )
        );

        let caller_paths = RuntimePaths::current(RuntimeMode::Shipyard);
        let error =
            ensure_canonical_recovery_paths(&caller_paths.global_dir, &caller_paths.state_dir)
                .expect_err("hostile caller paths must fail closed");
        assert!(
            error
                .message()
                .contains("cannot fork policy or attempt accounting")
        );
        return;
    }

    let canonical = canonical_recovery_paths().expect("stable account paths");
    let hostile_cwd = tempfile::tempdir().expect("hostile cwd");
    let hostile_home = hostile_cwd.path().join("attacker-home");
    fs::create_dir_all(&hostile_home).expect("hostile home");
    let test_name = concat!(
        module_path!(),
        "::canonical_recovery_authority_ignores_home_and_working_directory"
    );

    for home in [Some(hostile_home.as_os_str()), None] {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .current_dir(hostile_cwd.path())
            .env(PROBE_ENV, "1")
            .env(EXPECTED_GLOBAL_ENV, &canonical.global_dir)
            .env(EXPECTED_STATE_ENV, &canonical.state_dir)
            .env_remove("USERPROFILE");
        if let Some(home) = home {
            command.env("HOME", home);
        } else {
            command.env_remove("HOME");
        }
        let output = command.output().expect("run hostile-environment probe");
        assert!(
            output.status.success(),
            "hostile-environment probe failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn oversized_hostile_worker_failure_always_terminalizes_within_store_bound() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    let id = request.id.clone();
    store.enqueue(request.clone()).expect("enqueue");
    store
        .begin(&id, "worker-config", "generation")
        .expect("claim");
    let hostile = format!("{}{}", "x".repeat(16_384), "\u{1b}[31msecret\u{0}");
    let _ = fail_after_claim(&store, &request, "worker-config", hostile);
    let terminal = store.get(&id).expect("load").expect("record");
    assert_eq!(
        terminal.receipt.status,
        crate::recovery_worker::RecoveryStatus::Failed
    );
    let detail = terminal.receipt.detail.expect("failure detail");
    assert!(detail.len() <= MAX_RECEIPT_DETAIL_BYTES);
    assert!(!detail.chars().any(char::is_control));
}

#[test]
fn begin_error_after_durable_claim_marker_terminalizes_the_spent_attempt() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    store.enqueue(request.clone()).expect("enqueue");

    let mut interrupted = store
        .get(&request.id)
        .expect("load")
        .expect("pending record");
    let started_at = chrono::Utc::now();
    interrupted.receipt.status = RecoveryStatus::Running;
    interrupted.receipt.attempt = 1;
    interrupted.receipt.worker_generation = Some("generation".to_owned());
    interrupted.receipt.started_at = Some(started_at);
    interrupted.receipt.updated_at = started_at;
    store
        .persist_claim_for_test(&interrupted)
        .expect("durable claim marker");

    let error = terminal::recover_failed_begin(
        &store,
        &request,
        "worker-config",
        "generation",
        "failed to materialize running record",
    );
    assert!(error.message().contains("failed to materialize"));
    let durable = store.get(&request.id).expect("load").expect("record");
    assert_eq!(durable.receipt.status, RecoveryStatus::Failed);
    assert_eq!(durable.receipt.attempt, 1);
    assert_eq!(
        durable.receipt.worker_generation.as_deref(),
        Some("generation")
    );
}

#[test]
fn exhausted_deadline_after_claim_terminalizes_the_receipt() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    store.enqueue(request.clone()).expect("enqueue");
    let claim = terminal::ClaimedRecovery::begin(&store, &request, "worker-config", "generation")
        .expect("claim");
    let error = claim
        .run(|claim| {
            claim.worker_deadline(
                Instant::now()
                    .checked_sub(Duration::from_millis(1))
                    .expect("representable expired deadline"),
                30,
            )
        })
        .expect_err("expired deadline");
    assert!(error.message().contains("overall record deadline"));
    assert_eq!(
        store
            .get(&request.id)
            .expect("load")
            .expect("record")
            .receipt
            .status,
        crate::recovery_worker::RecoveryStatus::Failed
    );
}

#[test]
fn terminal_failure_uses_a_fresh_store_deadline_after_claim() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    store.enqueue(request.clone()).expect("enqueue");
    store
        .begin(&request.id, "worker-config", "generation")
        .expect("claim");
    let expired_store = store.clone().with_lock_deadline(
        Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("representable expired deadline"),
    );

    let error = fail_after_claim(
        &expired_store,
        &request,
        "worker-config",
        "post-claim reload failed",
    );
    assert!(error.message().contains("post-claim reload failed"));
    assert_eq!(
        store
            .get(&request.id)
            .expect("load")
            .expect("record")
            .receipt
            .status,
        crate::recovery_worker::RecoveryStatus::Failed
    );
}

#[test]
fn witness_rejects_same_head_policy_or_evidence_drift() {
    let temp = tempfile::tempdir().expect("tempdir");
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-a",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "policy-a",
        "worker-config",
    )
    .expect("request");
    write_recovery_witness(
        temp.path(),
        &request.repo,
        request.pr,
        &request.id,
        &request.head_sha,
        "policy-b",
        &request.failure_fingerprint,
    )
    .expect("witness");
    let error = verify_recovery_witness(temp.path(), &request).expect_err("drift rejected");
    assert!(error.message().contains("policy drifted"));
    remove_recovery_witness(temp.path(), &request.repo, request.pr)
        .expect("remove witness durably");
    remove_recovery_witness(temp.path(), &request.repo, request.pr)
        .expect("witness removal is idempotent");
    assert!(
        verify_recovery_witness(temp.path(), &request)
            .expect_err("removed witness rejects completion")
            .message()
            .contains("missing current deterministic recovery witness")
    );
}

#[test]
fn witnesses_for_two_prs_in_one_repository_do_not_invalidate_each_other() {
    let temp = tempfile::tempdir().expect("tempdir");
    let make = |pr| {
        RecoveryRequest::new(
            "Generous-Corp/pulp",
            pr,
            "main",
            "0123456789abcdef0123456789abcdef01234567",
            format!("failure-{pr}"),
            "required check failed",
            required_policy("macos"),
            vec![required_check("macos")],
            "steward-policy",
            "worker-config",
        )
        .expect("request")
    };
    let first = make(42);
    let second = make(43);
    for request in [&first, &second] {
        write_recovery_witness(
            temp.path(),
            &request.repo,
            request.pr,
            &request.id,
            &request.head_sha,
            &request.policy_signature,
            &request.failure_fingerprint,
        )
        .expect("witness");
    }
    verify_recovery_witness(temp.path(), &first).expect("first witness remains current");
    verify_recovery_witness(temp.path(), &second).expect("second witness remains current");
}

#[cfg(unix)]
#[test]
fn expired_absolute_worker_deadline_never_launches_the_model() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let worker = temp.path().join("worker.sh");
    fs::write(&worker, "#!/bin/sh\n: > launched\n").expect("worker fixture");
    let mut permissions = fs::metadata(&worker).expect("metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&worker, permissions).expect("executable fixture");
    let policy = RecoveryWorkerPolicy {
        enabled: true,
        provider: "local-test".to_owned(),
        first_line_model: DEFAULT_FIRST_LINE_MODEL.to_owned(),
        escalation_model: None,
        codex_binary: worker,
        codex_home: temp.path().join("codex-home"),
        timeout_seconds: 15,
        max_attempts_per_head: 1,
        max_log_tail_bytes: 4096,
        allowed_repositories: BTreeSet::new(),
        repo_paths: BTreeMap::new(),
    };
    let scratch = temp.path().join("scratch");
    let lease = acquire_global_model_lease(&temp.path().join("model.lock"))
        .expect("lease")
        .expect("uncontended lease");

    let output = run_worker_process(
        &policy,
        &serde_json::json!({"task": "must not launch"}),
        &lease,
        &scratch,
        Instant::now(),
    )
    .expect("expired deadline is a typed timeout");

    assert!(output.timed_out);
    assert_eq!(output.exit_code, None);
    assert!(!scratch.join("launched").exists());
}

#[cfg(unix)]
#[test]
fn supervised_worker_receives_request_on_stdin_and_returns_valid_json() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let worker = temp.path().join("worker.py");
    fs::write(
            &worker,
            r#"#!/usr/bin/env python3
import json, os, sys
request = json.load(sys.stdin)
assert request["task"].startswith("Route this exact-head failure")
assert os.path.basename(os.getcwd()) == "scratch"
for key in ["OPENROUTER_API_KEY", "HTTP_PROXY", "HTTPS_PROXY", "PIP_INDEX_URL", "DOCKER_CONFIG", "GITHUB_TOKEN", "SSH_AUTH_SOCK"]:
    assert key not in os.environ
json.dump({
  "schema_version": 1,
  "verdict": "escalate",
  "category": "compile",
  "confidence": "high",
  "evidence": [],
  "candidate_paths": [],
  "focused_tests": []
}, sys.stdout)
"#,
        )
        .expect("worker fixture");
    let mut permissions = fs::metadata(&worker).expect("metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&worker, permissions).expect("executable fixture");
    let policy = RecoveryWorkerPolicy {
        enabled: true,
        provider: "local-test".to_owned(),
        first_line_model: DEFAULT_FIRST_LINE_MODEL.to_owned(),
        escalation_model: None,
        codex_binary: worker,
        codex_home: temp.path().join("codex-home"),
        timeout_seconds: 15,
        max_attempts_per_head: 1,
        max_log_tail_bytes: 4096,
        allowed_repositories: BTreeSet::new(),
        repo_paths: BTreeMap::new(),
    };
    let request = serde_json::json!({"task": "Route this exact-head failure now"});
    let scratch = temp.path().join("scratch");
    let lease = acquire_global_model_lease(&temp.path().join("model.lock"))
        .expect("lease")
        .expect("uncontended lease");
    let output = run_worker_process(
        &policy,
        &request,
        &lease,
        &scratch,
        Instant::now() + Duration::from_secs(policy.timeout_seconds),
    )
    .expect("worker process");
    assert_eq!(output.exit_code, Some(0), "worker output: {output:?}");
    assert!(!output.timed_out);
    let parsed: RecoveryOutput = serde_json::from_slice(&output.stdout).expect("output JSON");
    parsed.validate().expect("valid output");
}
