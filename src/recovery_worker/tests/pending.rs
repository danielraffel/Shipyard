use super::*;

#[test]
fn pending_request_can_be_superseded_by_new_head() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let old = request(HEAD_A, "config-v1");
    let new = request(HEAD_B, "config-v1");
    store.enqueue(old.clone()).expect("old enqueue");
    store.enqueue(new.clone()).expect("new enqueue");

    let stale = store
        .supersede(&old.id, Some(&new.id), "pull-request head advanced")
        .expect("supersede");
    assert_eq!(stale.receipt.status, RecoveryStatus::Superseded);
    assert_eq!(
        stale.receipt.superseded_by.as_deref(),
        Some(new.id.as_str())
    );
    assert!(stale.receipt.completed_at.is_some());
}

#[test]
fn pending_enumeration_is_bounded_and_deterministic() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let mut later = request(HEAD_B, "config-v1");
    let mut earlier = request(HEAD_A, "config-v1");
    earlier.created_at = Utc::now() - chrono::Duration::seconds(1);
    later.created_at = Utc::now();
    store.enqueue(later.clone()).expect("later enqueue");
    store.enqueue(earlier.clone()).expect("earlier enqueue");

    let pending = store.pending(1).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].request.id, earlier.id);
    assert!(matches!(
        store.pending(MAX_PENDING_LIMIT + 1),
        Err(RecoveryError::InvalidRequest(_))
    ));
}

#[test]
fn preflight_deferral_rotates_pending_without_spending_an_attempt() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let mut first = request(HEAD_A, "config-v1");
    let mut second = request(HEAD_B, "config-v1");
    first.created_at = Utc::now() - chrono::Duration::seconds(2);
    second.created_at = Utc::now() - chrono::Duration::seconds(1);
    store.enqueue(first.clone()).expect("first enqueue");
    store.enqueue(second.clone()).expect("second enqueue");
    assert_eq!(store.pending(1).expect("oldest")[0].request.id, first.id);

    assert!(
        store
            .defer_pending(&first.id, "config-v1", "repository preflight unavailable")
            .expect("defer")
    );
    let pending = store.pending(2).expect("rotated pending");
    assert_eq!(pending[0].request.id, second.id);
    assert_eq!(pending[1].request.id, first.id);
    let deferred = store.get(&first.id).expect("load").expect("deferred");
    assert_eq!(deferred.receipt.status, RecoveryStatus::Pending);
    assert_eq!(deferred.receipt.attempt, 0);
    assert!(deferred.receipt.deferred_at.is_some());
    assert_eq!(
        deferred.receipt.detail.as_deref(),
        Some("repository preflight unavailable")
    );

    let mut reconfigured = first;
    reconfigured.config_signature = "config-v2".to_owned();
    assert_eq!(
        store.enqueue(reconfigured).expect("reactivate config"),
        EnqueueOutcome::Created
    );
    assert!(
        !store
            .defer_pending(&deferred.request.id, "config-v1", "stale worker failure")
            .expect("stale deferral ignored")
    );
    let current = store
        .get(&deferred.request.id)
        .expect("load")
        .expect("current");
    assert_eq!(current.request.config_signature, "config-v2");
    assert!(current.receipt.deferred_at.is_none());
    assert!(current.receipt.detail.is_none());
    let running = store
        .begin(&current.request.id, "config-v2", "worker-generation")
        .expect("claim reactivated request");
    assert_eq!(running.receipt.status, RecoveryStatus::Running);
    assert!(running.receipt.deferred_at.is_none());
    assert!(running.receipt.detail.is_none());
}

#[test]
fn read_only_pending_snapshot_never_creates_a_store_lock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let lock_path = temp.path().join("store.lock");
    assert!(!lock_path.exists());
    assert!(
        store
            .pending_read_only(1)
            .expect("empty snapshot")
            .is_empty()
    );
    assert!(!lock_path.exists());

    let request = request(HEAD_A, "config-v1");
    store.enqueue(request.clone()).expect("enqueue");
    let before = std::fs::metadata(&lock_path)
        .expect("lock metadata")
        .modified()
        .expect("modified time");
    assert_eq!(
        store
            .pending_read_only(1)
            .expect("populated snapshot")
            .first()
            .map(|record| record.request.id.as_str()),
        Some(request.id.as_str())
    );
    let after = std::fs::metadata(&lock_path)
        .expect("lock metadata")
        .modified()
        .expect("modified time");
    assert_eq!(before, after);
}

#[test]
fn stale_head_can_terminalize_without_a_known_successor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let stale = request(HEAD_A, "config-v1");
    store.enqueue(stale.clone()).expect("enqueue");

    let terminal = store
        .supersede(&stale.id, None, "live head no longer matches")
        .expect("supersede");
    assert_eq!(terminal.receipt.status, RecoveryStatus::Superseded);
    assert_eq!(terminal.receipt.superseded_by, None);
}

#[test]
fn reconciliation_fails_only_running_claims_older_than_cutoff() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let running = request(HEAD_A, "config-v1");
    let pending = request(HEAD_B, "config-v1");
    store.enqueue(running.clone()).expect("running enqueue");
    store.enqueue(pending.clone()).expect("pending enqueue");
    let claimed = store
        .begin(&running.id, "config-v1", "generation-a")
        .expect("begin");
    let started_at = claimed.receipt.started_at.expect("started at");

    assert!(
        store
            .reconcile_stale_running(started_at, "worker lease expired")
            .expect("exact cutoff")
            .is_empty()
    );
    let reconciled = store
        .reconcile_stale_running(
            started_at + chrono::Duration::nanoseconds(1),
            "worker lease expired",
        )
        .expect("reconcile");
    assert_eq!(reconciled.len(), 1);
    assert_eq!(reconciled[0].request.id, running.id);
    assert_eq!(reconciled[0].receipt.status, RecoveryStatus::Failed);
    assert_eq!(
        reconciled[0].receipt.detail.as_deref(),
        Some("worker lease expired")
    );
    assert_eq!(
        store
            .get(&pending.id)
            .expect("pending get")
            .expect("pending record")
            .receipt
            .status,
        RecoveryStatus::Pending
    );
    assert!(
        store
            .reconcile_stale_running(
                Utc::now() + chrono::Duration::days(1),
                "worker lease expired",
            )
            .expect("idempotent reconcile")
            .is_empty()
    );
}

#[test]
fn external_lease_proof_reconciles_recent_running_claims_immediately() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let running = request(HEAD_A, "config-v1");
    store.enqueue(running.clone()).expect("enqueue");
    store
        .begin(&running.id, "config-v1", "generation-a")
        .expect("begin");

    let reconciled = store
        .reconcile_orphaned_running("external lease proves prior worker exited")
        .expect("immediate reconciliation");
    assert_eq!(reconciled.len(), 1);
    assert_eq!(reconciled[0].request.id, running.id);
    assert_eq!(reconciled[0].receipt.status, RecoveryStatus::Failed);
    assert!(
        store
            .reconcile_orphaned_running("external lease proves prior worker exited")
            .expect("idempotent reconciliation")
            .is_empty()
    );
}

#[test]
fn reconciliation_rejects_hostile_detail_without_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let running = request(HEAD_A, "config-v1");
    store.enqueue(running.clone()).expect("enqueue");
    store
        .begin(&running.id, "config-v1", "generation-a")
        .expect("begin");

    assert!(matches!(
        store.reconcile_stale_running(Utc::now(), "hostile\0detail"),
        Err(RecoveryError::InvalidRequest(_))
    ));
    assert_eq!(
        store
            .get(&running.id)
            .expect("get")
            .expect("record")
            .receipt
            .status,
        RecoveryStatus::Running
    );
}
