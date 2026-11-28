use super::*;

fn minutes(count: i64) -> chrono::Duration {
    chrono::Duration::minutes(count)
}

#[test]
fn a_missing_or_old_run_is_stale_and_a_recent_one_is_not() {
    let now = Utc::now();
    assert!(worker_is_stale(None, now));
    assert!(worker_is_stale(
        Some(now - minutes(WORKER_STALE_AFTER_MINUTES)),
        now
    ));
    assert!(!worker_is_stale(
        Some(now - minutes(WORKER_STALE_AFTER_MINUTES - 1)),
        now
    ));
    // A marker from the future is stale beyond a few minutes of skew.
    assert!(!worker_is_stale(Some(now + minutes(4)), now));
    assert!(worker_is_stale(Some(now + minutes(6)), now));
}

#[test]
fn the_last_run_marker_round_trips_and_an_unreadable_one_is_no_run() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    assert_eq!(store.last_worker_run(), None);

    let at = Utc::now();
    store.record_worker_run(at).expect("record");
    assert_eq!(store.last_worker_run(), Some(at));

    fs::write(temp.path().join("worker-last-run"), b"not-json").expect("corrupt");
    assert_eq!(store.last_worker_run(), None);
}

#[test]
fn the_run_marker_is_not_mistaken_for_a_request_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    store
        .enqueue(request(HEAD_A, "config-v1"))
        .expect("enqueue");
    store.record_worker_run(Utc::now()).expect("record");
    assert_eq!(store.pending(10).expect("pending").len(), 1);
}

#[test]
fn the_oldest_unattempted_pending_request_ignores_attempted_ones() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    assert_eq!(store.oldest_unattempted_pending().expect("empty"), None);

    let attempted = request(HEAD_A, "config-v1");
    store.enqueue(attempted.clone()).expect("enqueue");
    store
        .begin(&attempted.id, "config-v1", "generation-a")
        .expect("claim");
    assert_eq!(store.oldest_unattempted_pending().expect("running"), None);

    let waiting = request(HEAD_B, "config-v1");
    let created = waiting.created_at;
    store.enqueue(waiting).expect("enqueue");
    assert_eq!(
        store.oldest_unattempted_pending().expect("waiting"),
        Some(created)
    );
}
