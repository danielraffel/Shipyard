use super::witness::remove_recovery_witness;
use super::*;

#[test]
fn contended_recovery_lease_respects_the_caller_deadline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store_root = temp.path().join("store");
    fs::create_dir_all(&store_root).expect("store root");
    let exclusive =
        acquire_recovery_enqueue_lease(&store_root, Instant::now() + Duration::from_secs(1))
            .expect("exclusive lease");

    let started = Instant::now();
    let error = acquire_recovery_enqueue_read_lease(
        &store_root,
        Instant::now() + Duration::from_millis(50),
    )
    .expect_err("shared lease must time out while contended");
    assert!(error.message().contains("timed out acquiring shared"));
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(exclusive);
}

#[test]
fn recovery_clear_preserves_a_newer_head_witness_across_the_operation_gap() {
    let first = tempfile::tempdir().expect("first state");
    let second = tempfile::tempdir().expect("second state");
    let repo = "Generous-Corp/pulp";
    let pr = 42;
    let head = "0123456789abcdef0123456789abcdef01234567";
    write_recovery_witness(
        first.path(),
        repo,
        pr,
        "first-request",
        head,
        "first-policy",
        "first-failure",
    )
    .expect("first witness");

    with_recovery_clear_fence(second.path(), repo, pr, head, || {
        write_recovery_witness(
            second.path(),
            repo,
            pr,
            "racing-request",
            "fedcba9876543210fedcba9876543210fedcba98",
            "racing-policy",
            "racing-failure",
        )
        .map_err(|error| error.message().to_owned())?;
        Ok(())
    })
    .expect("scoped two-phase clear");

    assert!(has_recovery_witness(first.path(), repo, pr).expect("first state"));
    assert!(has_recovery_witness(second.path(), repo, pr).expect("second state"));
    let payload = fs::read(super::witness::recovery_witness_path(
        second.path(),
        repo,
        pr,
    ))
    .expect("newer witness");
    let witness = serde_json::from_slice::<RecoveryWitness>(&payload).expect("witness JSON");
    assert_eq!(witness.head_sha, "fedcba9876543210fedcba9876543210fedcba98");
    remove_recovery_witness(first.path(), repo, pr).expect("cleanup first witness");
    remove_recovery_witness(second.path(), repo, pr).expect("cleanup second witness");
}

#[test]
fn recovery_clear_removes_a_same_head_witness_created_during_the_operation_gap() {
    let state = tempfile::tempdir().expect("state");
    let repo = "Generous-Corp/pulp";
    let pr = 42;
    let head = "0123456789abcdef0123456789abcdef01234567";

    with_recovery_clear_fence(state.path(), repo, pr, head, || {
        write_recovery_witness(
            state.path(),
            repo,
            pr,
            "racing-request",
            head,
            "racing-policy",
            "racing-failure",
        )
        .map_err(|error| error.message().to_owned())?;
        Ok(())
    })
    .expect("same-head clear");

    assert!(!has_recovery_witness(state.path(), repo, pr).expect("state"));
}

#[test]
fn recovery_clear_supersedes_active_record_without_a_witness() {
    let state = tempfile::tempdir().expect("state");
    let repo = "Generous-Corp/pulp";
    let pr = 42;
    let head = "0123456789abcdef0123456789abcdef01234567";
    let store = RecoveryStore::new(recovery_store_root(state.path())).expect("recovery store");
    let request = RecoveryRequest::new(
        repo,
        pr,
        "main",
        head,
        "failure-fingerprint",
        "pull request requires an exact-head update",
        Vec::new(),
        vec![RecoveryFailureFact::MergeState {
            state: "BEHIND".to_owned(),
        }],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    store
        .enqueue(request.clone())
        .expect("enqueue without witness");
    assert!(!has_recovery_witness(state.path(), repo, pr).expect("missing witness"));

    with_recovery_clear_fence(state.path(), repo, pr, head, || Ok(()))
        .expect("missing witness cannot bypass durable clear");

    let durable = store
        .get(&request.id)
        .expect("load")
        .expect("durable record");
    assert_eq!(durable.receipt.status, RecoveryStatus::Superseded);
}

#[test]
fn racing_clear_cannot_pass_record_and_witness_publication() {
    let state = tempfile::tempdir().expect("state");
    let state_path = state.path().to_path_buf();
    let repo = "Generous-Corp/pulp";
    let pr = 42;
    let head = "0123456789abcdef0123456789abcdef01234567";
    let store = RecoveryStore::new(recovery_store_root(&state_path)).expect("recovery store");
    let publication_lease =
        acquire_recovery_publication_lease(&state_path).expect("publication lease");
    let request = RecoveryRequest::new(
        repo,
        pr,
        "main",
        head,
        "failure-fingerprint",
        "pull request requires an exact-head update",
        Vec::new(),
        vec![RecoveryFailureFact::MergeState {
            state: "BEHIND".to_owned(),
        }],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    let request_id = request.id.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let clear = std::thread::spawn(move || {
        started_tx.send(()).expect("signal clear attempt");
        let result = with_recovery_clear_fence(&state_path, repo, pr, head, || Ok(()));
        done_tx.send(result).expect("return clear result");
    });
    started_rx.recv().expect("clear started");
    assert!(
        done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "clear must wait for the publication lease"
    );

    store.enqueue(request).expect("publish record");
    write_recovery_witness(
        state.path(),
        repo,
        pr,
        &request_id,
        head,
        "steward-policy",
        "failure-fingerprint",
    )
    .expect("publish witness");
    drop(publication_lease);
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("clear completed")
        .expect("clear result");
    clear.join().expect("clear thread");

    assert!(!has_recovery_witness(state.path(), repo, pr).expect("witness state"));
    let durable = store
        .get(&request_id)
        .expect("load")
        .expect("durable record");
    assert_eq!(durable.receipt.status, RecoveryStatus::Superseded);
}
