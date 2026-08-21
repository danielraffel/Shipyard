use super::*;

#[test]
fn default_attempt_budget_allows_only_one_claim() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let request = request(HEAD_A, "config-v1");
    let id = request.id.clone();
    store.enqueue(request).expect("enqueue");

    let running = store
        .begin(&id, "config-v1", "generation-a")
        .expect("begin");
    assert_eq!(running.receipt.attempt, 1);
    store
        .fail(&id, "config-v1", "worker exited nonzero")
        .expect("fail");
    assert!(matches!(
        store.begin(&id, "config-v1", "generation-b"),
        Err(RecoveryError::AttemptsExhausted {
            max_attempts: 1,
            ..
        })
    ));
}

#[test]
fn interrupted_atomic_claim_marker_fences_replay_without_corrupting_the_queue() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let claimed = request(HEAD_A, "config-v1");
    let unrelated = request(HEAD_B, "config-v1");
    let path = temp.path().join(format!("{}.json", claimed.id));
    store.enqueue(claimed.clone()).expect("enqueue claim");
    store.enqueue(unrelated.clone()).expect("enqueue unrelated");

    let mut interrupted = store
        .get(&claimed.id)
        .expect("load")
        .expect("pending record");
    let started_at = Utc::now();
    interrupted.receipt.status = RecoveryStatus::Running;
    interrupted.receipt.attempt = 1;
    interrupted.receipt.worker_generation = Some("worker-generation".to_owned());
    interrupted.receipt.started_at = Some(started_at);
    interrupted.receipt.updated_at = started_at;
    store
        .persist_claim_unlocked(&interrupted)
        .expect("durable claim marker");

    let on_disk = serde_json::from_slice::<RecoveryRecord>(
        &std::fs::read(&path).expect("original record remains readable"),
    )
    .expect("pending JSON was never truncated");
    assert_eq!(on_disk.receipt.status, RecoveryStatus::Pending);
    let recovered = store
        .get(&claimed.id)
        .expect("claim recovery")
        .expect("claimed record");
    assert_eq!(recovered.receipt.status, RecoveryStatus::Running);
    assert_eq!(recovered.receipt.attempt, 1);
    assert_eq!(
        store
            .pending(10)
            .expect("unrelated queue remains readable")
            .into_iter()
            .map(|record| record.request.id)
            .collect::<Vec<_>>(),
        vec![unrelated.id]
    );

    store
        .begin(&claimed.id, &claimed.config_signature, "worker-generation")
        .expect("idempotent claim materialization");
    assert_eq!(
        store
            .get(&claimed.id)
            .expect("load")
            .expect("record")
            .receipt
            .attempt,
        1
    );
}
