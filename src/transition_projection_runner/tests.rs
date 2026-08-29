#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};

use super::*;
use crate::config::LocalOverlaySource;
use crate::transition_projection::{ProjectionEvidence, TransitionKind};

fn inert_executable() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("C:/Windows/System32/where.exe")
    } else {
        PathBuf::from("/bin/false")
    }
}

fn draft(receipt: &[u8], sequence: u64, kind: TransitionKind) -> TransitionDraft {
    TransitionDraft {
        workstream_id: "GEN-14".to_owned(),
        sequence,
        kind,
        evidence: ProjectionEvidence {
            source_revision: "a".repeat(64),
            exact_head: Some("b".repeat(40)),
            receipt_sha256: hex::encode(Sha256::digest(receipt)),
        },
        supersedes_transition_id: None,
        note: Some("safe".to_owned()),
    }
}

fn policy(executable: &Path, digest: &str, secret: &Path) -> String {
    format!(
        "[transition_projection]\nenabled = true\nexecutable_path = \"{}\"\nexecutable_sha256 = \"{digest}\"\nargv = [\"linear-v1\"]\ndeadline_seconds = 2\nmax_stdout_bytes = 4096\nmax_stderr_bytes = 4096\nrepositories = [\"owner/repo\"]\n[transition_projection.secret_files]\nLINEAR_API_KEY_FILE = \"{}\"\n",
        executable.display(),
        secret.display()
    )
}

#[test]
fn disabled_and_unavailable_have_zero_stewardship_effect() {
    let temp = tempfile::tempdir().unwrap();
    let config = trusted_projection_runner_config(RuntimeMode::Shipyard, temp.path().into())
        .expect("absent policy");
    assert_eq!(config, None);
    let runtime = TransitionProjectionRuntime::for_daemon(
        RuntimeMode::Shipyard,
        temp.path().into(),
        temp.path().join("state"),
    );
    let result = runtime.ingress().enqueue_after_commit(
        "not/a/repo",
        draft(b"missing", 1, TransitionKind::Handoff),
        Path::new("/missing"),
    );
    assert_eq!(result.unwrap(), EnqueueOutcome::Disabled);
    assert!(!temp.path().join("state/transition-projection").exists());
}

#[test]
fn protected_config_ignores_overlays_and_rejects_secret_argv() {
    let temp = tempfile::tempdir().unwrap();
    let global = temp.path().join("global");
    let project = temp.path().join("project");
    fs::create_dir_all(&global).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("config.toml"),
        "[transition_projection]\nenabled=true\n",
    )
    .unwrap();
    let loaded = LoadedConfig::load(
        Some(global.clone()),
        Some(project),
        None,
        LocalOverlaySource::None,
    )
    .unwrap();
    assert!(loaded.get(POLICY_KEY).is_some());
    assert_eq!(
        trusted_projection_runner_config(RuntimeMode::Shipyard, global).unwrap(),
        None
    );

    let secret = temp.path().join("linear-key");
    fs::write(&secret, b"not-read-at-config-time").unwrap();
    fs::write(
        temp.path().join("config.toml"),
        policy(&inert_executable(), &"a".repeat(64), &secret),
    )
    .unwrap();
    assert!(
        trusted_projection_runner_config(RuntimeMode::Shipyard, temp.path().into())
            .unwrap()
            .is_some()
    );

    let over_lease = policy(&inert_executable(), &"a".repeat(64), &secret)
        .replace("deadline_seconds = 2", "deadline_seconds = 28");
    fs::write(temp.path().join("config.toml"), over_lease).unwrap();
    assert!(trusted_projection_runner_config(RuntimeMode::Shipyard, temp.path().into()).is_err());

    let bad = "[transition_projection]\nenabled=true\nexecutable_path=\"/bin/x\"\nexecutable_sha256=\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nargv=[\"token=secret\"]\ndeadline_seconds=1\nmax_stdout_bytes=1\nmax_stderr_bytes=1\nrepositories=[\"owner/repo\"]\n";
    fs::write(temp.path().join("config.toml"), bad).unwrap();
    assert!(trusted_projection_runner_config(RuntimeMode::Shipyard, temp.path().into()).is_err());
}

#[test]
#[cfg(unix)]
fn commit_before_enqueue_and_repository_partition_are_enforced() {
    let temp = tempfile::tempdir().unwrap();
    let receipt_path = temp.path().join("receipt.json");
    fs::write(&receipt_path, b"committed-receipt").unwrap();
    fs::set_permissions(&receipt_path, fs::Permissions::from_mode(0o600)).unwrap();
    let config = ProjectionRunnerConfig {
        executable_path: "/bin/false".into(),
        executable_sha256: "a".repeat(64),
        argv: vec!["linear-v1".into()],
        secret_files: BTreeMap::new(),
        deadline_seconds: 1,
        max_stdout_bytes: 1024,
        max_stderr_bytes: 1024,
        repositories: BTreeSet::from(["owner/repo".to_owned()]),
    };
    let ingress = CommittedTransitionIngress::enabled(temp.path(), &config);
    assert!(
        ingress
            .enqueue_after_commit(
                "other/repo",
                draft(b"committed-receipt", 1, TransitionKind::Waiting),
                &receipt_path,
            )
            .is_err()
    );
    assert!(
        ingress
            .enqueue_after_commit(
                "owner/repo",
                draft(b"wrong", 1, TransitionKind::Waiting),
                &receipt_path,
            )
            .is_err()
    );
    assert_eq!(
        ingress
            .enqueue_after_commit(
                "owner/repo",
                draft(b"committed-receipt", 1, TransitionKind::Waiting),
                &receipt_path,
            )
            .unwrap(),
        EnqueueOutcome::Queued
    );
    let expected = repository_outbox(&temp.path().join("transition-projection"), "owner/repo");
    assert!(expected.join("transitions.ndjson").is_file());
    assert!(
        !temp
            .path()
            .join("transition-projection/repositories/owner/repo")
            .exists()
    );
}

#[test]
#[cfg(unix)]
fn every_allowed_transition_kind_uses_the_same_committed_ingress() {
    let temp = tempfile::tempdir().unwrap();
    let config = ProjectionRunnerConfig {
        executable_path: "/bin/false".into(),
        executable_sha256: "a".repeat(64),
        argv: vec!["linear-v1".into()],
        secret_files: BTreeMap::new(),
        deadline_seconds: 1,
        max_stdout_bytes: 1024,
        max_stderr_bytes: 1024,
        repositories: BTreeSet::from(["owner/repo".to_owned()]),
    };
    let ingress = CommittedTransitionIngress::enabled(temp.path(), &config);
    for (index, kind) in [
        TransitionKind::Handoff,
        TransitionKind::Waiting,
        TransitionKind::Actionable,
        TransitionKind::NewHead,
        TransitionKind::Merge,
        TransitionKind::ConfiguredClosure,
    ]
    .into_iter()
    .enumerate()
    {
        let bytes = format!("receipt-{index}");
        let path = temp.path().join(format!("receipt-{index}.json"));
        fs::write(&path, bytes.as_bytes()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            ingress
                .enqueue_after_commit(
                    "owner/repo",
                    draft(bytes.as_bytes(), index as u64 + 1, kind),
                    &path
                )
                .unwrap(),
            EnqueueOutcome::Queued
        );
    }
}

#[test]
#[cfg(unix)]
fn sqlite_intent_drainer_recovers_append_before_mark_idempotently() {
    use crate::work_ledger::{
        RepoPolicy, native_publication_test_policy as native_policy,
        native_publication_test_request as request,
    };

    let temp = tempfile::tempdir().expect("state");
    let request = request();
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    ledger
        .set_repo_policy(
            &RepoPolicy {
                repo: request.repository.clone(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                revision: 0,
            },
            0,
        )
        .expect("policy");
    WorkLedger::plan_or_apply_native_continuation(
        temp.path(),
        &request,
        &native_policy(vec![request.repository.clone()]),
        true,
    )
    .expect("publication");
    let config = ProjectionRunnerConfig {
        executable_path: inert_executable(),
        executable_sha256: "a".repeat(64),
        argv: vec!["linear-v1".into()],
        secret_files: BTreeMap::new(),
        deadline_seconds: 1,
        max_stdout_bytes: 1024,
        max_stderr_bytes: 1024,
        repositories: BTreeSet::from([request.repository.clone()]),
    };
    let ingress = CommittedTransitionIngress::enabled(temp.path(), &config);
    let intent = ledger
        .pending_projection_intents(0, 1)
        .expect("pending")
        .pop()
        .expect("managed intent");
    let draft = intent.reconstruct().expect("draft");
    assert_eq!(
        ingress
            .enqueue_committed_snapshot(&intent.repository, draft, &intent.receipt_snapshot,)
            .expect("append"),
        EnqueueOutcome::Queued,
    );
    // Simulate a crash after append and before the SQLite projected mark.
    let report = drain_committed_projection_intents(temp.path(), &ingress, 0);
    assert_eq!(report.projected, 1);
    assert!(!report.has_failures());
    let state = ledger
        .projection_intent_state(&intent.intent_id)
        .expect("intent state");
    assert_eq!(state, ("projected".to_owned(), 1));
    let snapshot = open_repository_outbox(
        &temp.path().join("transition-projection"),
        &request.repository,
    )
    .expect("outbox")
    .snapshot()
    .expect("snapshot");
    assert_eq!(snapshot.len(), 1);
}

#[test]
#[cfg(unix)]
fn corrupt_workstream_is_quarantined_without_starving_another() {
    use crate::work_ledger::{
        RepoPolicy, native_publication_test_policy as native_policy,
        native_publication_test_request as request,
    };

    let temp = tempfile::tempdir().expect("state");
    let first = request();
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    ledger
        .set_repo_policy(
            &RepoPolicy {
                repo: first.repository.clone(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                revision: 0,
            },
            0,
        )
        .expect("policy");
    let policy = native_policy(vec![first.repository.clone()]);
    WorkLedger::plan_or_apply_native_continuation(temp.path(), &first, &policy, true)
        .expect("first publication");
    let mut second = first.clone();
    second.pull_request = 44;
    second.head_sha = "c".repeat(40);
    second.workstream_handle = "GEN-44".to_owned();
    second.context_url = Some("https://linear.example/GEN-44".to_owned());
    second.plan_sha256 = "d".repeat(64);
    WorkLedger::plan_or_apply_native_continuation(temp.path(), &second, &policy, true)
        .expect("second publication");
    let intents = ledger.pending_projection_intents(0, 8).expect("intents");
    assert_eq!(intents.len(), 2);
    let corrupt = intents
        .iter()
        .find(|intent| intent.reconstruct().expect("draft").workstream_id == "GEN-43")
        .expect("corrupt target");
    ledger
        .corrupt_projection_receipt_for_test(&corrupt.intent_id)
        .expect("corrupt snapshot");
    let config = ProjectionRunnerConfig {
        executable_path: "/bin/false".into(),
        executable_sha256: "a".repeat(64),
        argv: vec!["linear-v1".into()],
        secret_files: BTreeMap::new(),
        deadline_seconds: 1,
        max_stdout_bytes: 1024,
        max_stderr_bytes: 1024,
        repositories: BTreeSet::from([first.repository.clone()]),
    };
    let ingress = CommittedTransitionIngress::enabled(temp.path(), &config);
    let report = drain_committed_projection_intents(temp.path(), &ingress, 0);
    assert_eq!(report.quarantined, 1);
    assert_eq!(report.projected, 1);
    assert!(!report.has_failures());
    assert_eq!(
        report.diagnostic_error().as_deref(),
        Some("transition-projection-intent-contradiction")
    );
    assert_eq!(
        ledger
            .projection_intent_state(&corrupt.intent_id)
            .expect("corrupt state")
            .0,
        "quarantined",
    );
    let healthy = intents
        .iter()
        .find(|intent| intent.intent_id != corrupt.intent_id)
        .expect("healthy");
    assert_eq!(
        ledger
            .projection_intent_state(&healthy.intent_id)
            .expect("healthy state")
            .0,
        "projected",
    );
}

#[test]
fn daemon_status_preserves_intent_drain_failure_after_outbox_success() {
    let mut status = ProjectionRunnerStatus {
        enabled: true,
        ..ProjectionRunnerStatus::default()
    };
    let mut intent_drain = ProjectionIntentDrainReport::default();
    intent_drain.record_failure(
        ProjectionDrainFailureKind::StateMutation,
        Some("opaque-intent"),
        "private database detail",
    );
    apply_worker_report(
        &mut status,
        ProjectionWorkerReport {
            intent_drain,
            reconciliations: vec![Ok(ReconcileOutcome::Idle)],
        },
    );
    assert_eq!(
        status.last_error.as_deref(),
        Some("transition-projection-intent-drain-state-mutation")
    );
    assert_eq!(status.last_outcome.as_deref(), Some("idle"));
    assert!(!status.last_error.unwrap().contains("private"));
}

#[test]
fn malformed_protocol_response_is_retryable_and_secret_free() {
    let failure = serde_json::from_slice::<ProtocolResponse>(b"{not-json")
        .map_err(|_| adapter_failure("malformed-response token=secret", true))
        .unwrap_err();
    assert!(failure.retryable);
    assert!(!failure.reason.contains("secret"));
    assert!(!failure.reason.contains("token"));
}

#[test]
#[cfg(unix)]
fn daemon_restart_reopens_same_repository_outbox() {
    let temp = tempfile::tempdir().unwrap();
    let receipt_path = temp.path().join("receipt");
    fs::write(&receipt_path, b"receipt").unwrap();
    fs::set_permissions(&receipt_path, fs::Permissions::from_mode(0o600)).unwrap();
    let config = ProjectionRunnerConfig {
        executable_path: "/bin/false".into(),
        executable_sha256: "a".repeat(64),
        argv: vec!["linear-v1".into()],
        secret_files: BTreeMap::new(),
        deadline_seconds: 1,
        max_stdout_bytes: 1024,
        max_stderr_bytes: 1024,
        repositories: BTreeSet::from(["owner/repo".to_owned()]),
    };
    CommittedTransitionIngress::enabled(temp.path(), &config)
        .enqueue_after_commit(
            "owner/repo",
            draft(b"receipt", 1, TransitionKind::Actionable),
            &receipt_path,
        )
        .unwrap();
    let root = temp.path().join("transition-projection");
    let first = TransitionOutbox::open(repository_outbox(&root, "owner/repo")).unwrap();
    let second = TransitionOutbox::open(repository_outbox(&root, "owner/repo")).unwrap();
    assert_eq!(first.snapshot().unwrap(), second.snapshot().unwrap());
}

#[test]
fn crash_after_external_acceptance_replays_same_key_then_acks() {
    struct AcceptedThenReadable {
        accepted: Arc<Mutex<BTreeMap<String, String>>>,
        fail_readback_once: bool,
    }

    impl TransitionProjectionAdapter for AcceptedThenReadable {
        fn submit(
            &mut self,
            transition: &ProjectedTransition,
        ) -> Result<SubmitReceipt, AdapterFailure> {
            self.accepted
                .lock()
                .unwrap()
                .entry(transition.transition_id.clone())
                .or_insert_with(|| transition.evidence_identity.clone());
            Ok(SubmitReceipt {
                external_id: "external-1".into(),
                idempotency_key: transition.transition_id.clone(),
            })
        }

        fn readback(
            &mut self,
            receipt: &SubmitReceipt,
        ) -> Result<ProjectionReadback, AdapterFailure> {
            if self.fail_readback_once {
                self.fail_readback_once = false;
                return Err(adapter_failure("accepted-before-crash", true));
            }
            Ok(ProjectionReadback {
                transition_id: receipt.idempotency_key.clone(),
                evidence_identity: self.accepted.lock().unwrap()[&receipt.idempotency_key].clone(),
            })
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("outbox");
    let outbox = TransitionOutbox::open(&root).unwrap();
    outbox
        .enqueue(draft(b"receipt", 1, TransitionKind::Merge))
        .unwrap();
    let accepted = Arc::new(Mutex::new(BTreeMap::new()));
    let mut first = AcceptedThenReadable {
        accepted: Arc::clone(&accepted),
        fail_readback_once: true,
    };
    assert!(matches!(
        outbox.reconcile_one(&mut first, 1).unwrap(),
        ReconcileOutcome::RetryQueued { .. }
    ));
    drop(outbox);
    let reopened = TransitionOutbox::open(root).unwrap();
    let mut recovered = AcceptedThenReadable {
        accepted: Arc::clone(&accepted),
        fail_readback_once: false,
    };
    assert!(matches!(
        reopened.reconcile_one(&mut recovered, 1_001).unwrap(),
        ReconcileOutcome::Acknowledged { .. }
    ));
    assert_eq!(accepted.lock().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn companion_receives_descriptor_bound_secret_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let secret = temp.path().join("linear-key");
    fs::write(&secret, b"original-secret").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let snapshot_dir = temp.path().join("snapshot");
    fs::create_dir(&snapshot_dir).unwrap();
    fs::set_permissions(&snapshot_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let config = ProjectionRunnerConfig {
        executable_path: "/bin/false".into(),
        executable_sha256: "a".repeat(64),
        argv: Vec::new(),
        secret_files: BTreeMap::from([("LINEAR_API_KEY_FILE".to_owned(), secret.clone())]),
        deadline_seconds: 1,
        max_stdout_bytes: 1024,
        max_stderr_bytes: 1024,
        repositories: BTreeSet::from(["owner/repo".to_owned()]),
    };
    let environment = snapshot_secret_files(
        &config,
        &snapshot_dir,
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    fs::write(secret, b"replacement-secret").unwrap();
    let snapshot = PathBuf::from(&environment["LINEAR_API_KEY_FILE"]);
    assert_eq!(fs::read(snapshot).unwrap(), b"original-secret");
}

#[cfg(unix)]
#[test]
fn companion_timeout_is_bounded_and_descendant_safe() {
    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join("projection-adapter");
    let source = temp.path().join("projection-adapter.c");
    fs::write(
        &source,
        "#include <unistd.h>\nint main(void) { sleep(5); return 0; }\n",
    )
    .unwrap();
    assert!(
        Command::new("/usr/bin/cc")
            .args(["-o"])
            .arg(&executable)
            .arg(&source)
            .status()
            .unwrap()
            .success()
    );
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o500)).unwrap();
    let digest = hex::encode(Sha256::digest(fs::read(&executable).unwrap()));
    let config = ProjectionRunnerConfig {
        executable_path: executable,
        executable_sha256: digest,
        argv: vec!["5".into()],
        secret_files: BTreeMap::new(),
        deadline_seconds: 1,
        max_stdout_bytes: 1024,
        max_stderr_bytes: 1024,
        repositories: BTreeSet::from(["owner/repo".to_owned()]),
    };
    let started = Instant::now();
    assert_eq!(
        run_companion(&config, b"{}"),
        Err("companion-timeout-or-output-limit")
    );
    assert!(started.elapsed() < Duration::from_secs(4));
}
