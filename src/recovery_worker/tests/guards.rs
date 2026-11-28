use super::*;

const CAPS: AttemptCaps = AttemptCaps {
    per_hour: 2,
    per_pr_per_day: 1,
};

fn admit(store: &RecoveryStore, pr: u64, now: DateTime<Utc>, record: bool) -> AttemptAdmission {
    store
        .admit_attempt("Generous-Corp/pulp", pr, now, CAPS, record)
        .expect("admit")
}

#[test]
fn the_hour_cap_refuses_a_third_attempt_and_frees_after_an_hour() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let now = Utc::now();
    assert_eq!(admit(&store, 1, now, true), AttemptAdmission::Admitted);
    assert_eq!(admit(&store, 2, now, true), AttemptAdmission::Admitted);
    assert_eq!(
        admit(&store, 3, now, true),
        AttemptAdmission::HourCapReached { used: 2, cap: 2 }
    );
    let later = now + chrono::Duration::minutes(61);
    assert_eq!(admit(&store, 3, later, true), AttemptAdmission::Admitted);
}

#[test]
fn the_pr_cap_counts_every_head_of_one_pull_request_for_a_day() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let now = Utc::now();
    assert_eq!(admit(&store, 7, now, true), AttemptAdmission::Admitted);
    let later = now + chrono::Duration::hours(2);
    assert_eq!(
        admit(&store, 7, later, true),
        AttemptAdmission::PrCapReached { used: 1, cap: 1 }
    );
    // Another pull request is unaffected, and the day window expires.
    assert_eq!(admit(&store, 8, later, true), AttemptAdmission::Admitted);
    let next_day = now + chrono::Duration::hours(25);
    assert_eq!(admit(&store, 7, next_day, true), AttemptAdmission::Admitted);
}

#[test]
fn a_check_without_record_spends_nothing_and_a_corrupt_ledger_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let now = Utc::now();
    for _ in 0..5 {
        assert_eq!(admit(&store, 1, now, false), AttemptAdmission::Admitted);
    }
    fs::write(temp.path().join("attempt-ledger"), b"not-json").expect("corrupt");
    store
        .admit_attempt("Generous-Corp/pulp", 1, now, CAPS, true)
        .expect_err("an unreadable ledger must not admit");
}

#[test]
fn a_running_request_fences_every_other_request_on_its_pull_request() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let claimed = request(HEAD_A, "config-v1");
    let newer = request(HEAD_B, "config-v1");
    store.enqueue(claimed.clone()).expect("enqueue claimed");
    store.enqueue(newer.clone()).expect("enqueue newer");
    assert_eq!(
        store
            .active_claim_on_pull(&newer.repo, newer.pr, &newer.id)
            .expect("fence"),
        None
    );
    store
        .begin(&claimed.id, "config-v1", "generation-a")
        .expect("claim");
    assert_eq!(
        store
            .active_claim_on_pull(&newer.repo, newer.pr, &newer.id)
            .expect("fence"),
        Some(claimed.id.clone())
    );
    assert_eq!(
        store
            .active_claim_on_pull(&claimed.repo, claimed.pr, &claimed.id)
            .expect("own claim is not a fence"),
        None
    );
}
