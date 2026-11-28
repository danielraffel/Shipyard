use super::*;
use std::collections::BTreeMap;

const BASE: &str = "main";

fn age() -> chrono::Duration {
    chrono::Duration::days(MAX_PENDING_AGE_DAYS)
}

fn request_for(pr: u64, head: &str) -> RecoveryRequest {
    RecoveryRequest::new(
        "Generous-Corp/pulp",
        pr,
        BASE,
        head,
        "failure-v1",
        "Required Windows check failed at the exact head.",
        required_policy("windows"),
        vec![required_check("windows")],
        "policy-v1",
        "config-v1",
    )
    .expect("request")
}

fn heads(rows: &[(u64, &str)]) -> BTreeMap<u64, String> {
    rows.iter()
        .map(|(pr, head)| (*pr, (*head).to_owned()))
        .collect()
}

fn status(store: &RecoveryStore, id: &str) -> RecoveryStatus {
    store.get(id).expect("get").expect("record").receipt.status
}

#[test]
fn dead_pending_records_are_retired_and_the_live_one_is_kept() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let live = request_for(1, HEAD_A);
    let closed = request_for(2, HEAD_A);
    let moved = request_for(3, HEAD_A);
    for request in [&live, &closed, &moved] {
        store.enqueue(request.clone()).expect("enqueue");
    }

    let outcome = store
        .prune_unattempted(
            "Generous-Corp/pulp",
            BASE,
            &heads(&[(1, HEAD_A), (3, HEAD_B)]),
            Utc::now(),
            age(),
        )
        .expect("prune");

    assert_eq!(outcome.retired_dead, 2);
    assert_eq!(outcome.retired_aged, 0);
    assert_eq!(outcome.active, 1);
    assert_eq!(status(&store, &live.id), RecoveryStatus::Pending);
    assert_eq!(status(&store, &closed.id), RecoveryStatus::Superseded);
    assert_eq!(status(&store, &moved.id), RecoveryStatus::Superseded);
    assert_eq!(store.pending(10).expect("pending").len(), 1);
}

#[test]
fn aged_unattempted_records_are_retired_even_when_the_head_is_live() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let live = request_for(1, HEAD_A);
    store.enqueue(live.clone()).expect("enqueue");
    let later = Utc::now() + chrono::Duration::days(MAX_PENDING_AGE_DAYS + 1);

    let outcome = store
        .prune_unattempted(
            "Generous-Corp/pulp",
            BASE,
            &heads(&[(1, HEAD_A)]),
            later,
            age(),
        )
        .expect("prune");

    assert_eq!((outcome.retired_dead, outcome.retired_aged), (0, 1));
    assert_eq!(status(&store, &live.id), RecoveryStatus::Superseded);
}

#[test]
fn attempted_and_other_repo_records_are_never_retired_as_dead() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let running = request_for(5, HEAD_A);
    store.enqueue(running.clone()).expect("enqueue");
    store
        .begin(&running.id, "config-v1", "generation-a")
        .expect("claim");

    let outcome = store
        .prune_unattempted(
            "Generous-Corp/pulp",
            BASE,
            &BTreeMap::new(),
            Utc::now(),
            age(),
        )
        .expect("prune");
    assert_eq!(outcome.retired_dead, 0);
    assert_eq!(status(&store, &running.id), RecoveryStatus::Running);

    let other = request_for(6, HEAD_A);
    store.enqueue(other.clone()).expect("enqueue other");
    let outcome = store
        .prune_unattempted(
            "Generous-Corp/other",
            BASE,
            &BTreeMap::new(),
            Utc::now(),
            age(),
        )
        .expect("prune other repo");
    assert_eq!(outcome.retired_dead, 0);
    assert_eq!(status(&store, &other.id), RecoveryStatus::Pending);
}

#[test]
fn a_retired_head_that_turns_red_again_is_accepted_again() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let request = request_for(1, HEAD_A);
    store.enqueue(request.clone()).expect("enqueue");
    store
        .prune_unattempted(
            "Generous-Corp/pulp",
            BASE,
            &BTreeMap::new(),
            Utc::now(),
            age(),
        )
        .expect("prune");

    assert_eq!(
        store.enqueue(request.clone()).expect("re-enqueue"),
        EnqueueOutcome::Created
    );
    assert_eq!(status(&store, &request.id), RecoveryStatus::Pending);
}

#[test]
fn a_store_full_of_dead_records_accepts_a_new_request_only_after_pruning() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    for pr in 0..super::super::archive::MAX_ACTIVE_RECORDS as u64 {
        // Written directly: enqueue re-scans the hot set, which is quadratic.
        let request = request_for(1000 + pr, HEAD_A);
        let now = Utc::now();
        store
            .save_unlocked(&RecoveryRecord {
                schema_version: RECOVERY_SCHEMA_VERSION,
                receipt: RecoveryReceipt {
                    schema_version: RECOVERY_SCHEMA_VERSION,
                    request_id: request.id.clone(),
                    status: RecoveryStatus::Pending,
                    attempt: 0,
                    max_attempts: 1,
                    config_signature: request.config_signature.clone(),
                    worker_generation: None,
                    started_at: None,
                    completed_at: None,
                    deferred_at: None,
                    updated_at: now,
                    superseded_by: None,
                    detail: None,
                    output: None,
                },
                request,
            })
            .expect("fill");
    }
    let fresh = request_for(1, HEAD_B);
    let error = store.enqueue(fresh.clone()).expect_err("store is full");
    assert!(error.to_string().contains("active-record limit"));

    let outcome = store
        .prune_unattempted(
            "Generous-Corp/pulp",
            BASE,
            &heads(&[(1, HEAD_B)]),
            Utc::now(),
            age(),
        )
        .expect("prune");
    assert_eq!(
        outcome.retired_dead,
        super::super::archive::MAX_ACTIVE_RECORDS
    );
    assert_eq!(outcome.active, 0);
    assert!(!outcome.full());
    assert_eq!(
        store.enqueue(fresh).expect("accepted"),
        EnqueueOutcome::Created
    );
}

fn other_request(repo: &str, base: &str, pr: u64) -> RecoveryRequest {
    RecoveryRequest::new(
        repo,
        pr,
        base,
        HEAD_A,
        "failure-v1",
        "Required Windows check failed at the exact head.",
        required_policy("windows"),
        vec![required_check("windows")],
        "policy-v1",
        "config-v1",
    )
    .expect("request")
}

#[test]
fn a_record_on_another_base_is_kept() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let release = other_request("Generous-Corp/pulp", "release", 1);
    store.enqueue(release.clone()).expect("enqueue");

    let outcome = store
        .prune_unattempted(
            "Generous-Corp/pulp",
            BASE,
            &BTreeMap::new(),
            Utc::now(),
            age(),
        )
        .expect("prune");

    assert_eq!(outcome.retired_dead, 0);
    assert_eq!(status(&store, &release.id), RecoveryStatus::Pending);
}

#[test]
fn the_age_bound_covers_other_repos_but_never_a_running_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let idle = other_request("Generous-Corp/forge", BASE, 1);
    let running = other_request("Generous-Corp/forge", BASE, 2);
    store.enqueue(idle.clone()).expect("enqueue idle");
    store.enqueue(running.clone()).expect("enqueue running");
    store
        .begin(&running.id, "config-v1", "generation-a")
        .expect("claim");
    let later = Utc::now() + chrono::Duration::days(MAX_PENDING_AGE_DAYS + 1);

    let outcome = store
        .prune_unattempted("Generous-Corp/pulp", BASE, &BTreeMap::new(), later, age())
        .expect("prune");

    assert_eq!((outcome.retired_dead, outcome.retired_aged), (0, 1));
    assert_eq!(status(&store, &idle.id), RecoveryStatus::Superseded);
    assert_eq!(status(&store, &running.id), RecoveryStatus::Running);
}

#[test]
fn an_observed_row_without_a_full_head_does_not_retire_its_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let request = request_for(1, HEAD_A);
    store.enqueue(request.clone()).expect("enqueue");

    for missing in ["", "abc123"] {
        let outcome = store
            .prune_unattempted(
                "Generous-Corp/pulp",
                BASE,
                &heads(&[(1, missing)]),
                Utc::now(),
                age(),
            )
            .expect("prune");
        assert_eq!(outcome.retired_dead, 0, "head {missing:?}");
    }
    assert_eq!(status(&store, &request.id), RecoveryStatus::Pending);
}

#[test]
fn one_unreadable_record_does_not_stop_the_sweep() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let dead = request_for(1, HEAD_A);
    store.enqueue(dead.clone()).expect("enqueue");
    // Sorts before every sha256 id that starts with a digit or letter.
    let bad = format!("{}.json", "0".repeat(64));
    fs::write(temp.path().join(bad), b"not-json").expect("corrupt record");

    let outcome = store
        .prune_unattempted(
            "Generous-Corp/pulp",
            BASE,
            &BTreeMap::new(),
            Utc::now(),
            age(),
        )
        .expect("prune");

    assert_eq!(outcome.unreadable, 1);
    assert_eq!(outcome.retired_dead, 1);
    assert_eq!(status(&store, &dead.id), RecoveryStatus::Superseded);
}
