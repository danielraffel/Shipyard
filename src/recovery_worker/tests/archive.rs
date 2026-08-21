use super::*;

#[test]
fn terminal_receipts_move_out_of_the_bounded_hot_set_and_remove_claims() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let request = request(HEAD_A, "config-v1");
    let id = request.id.clone();
    store.enqueue(request).expect("enqueue");
    store
        .begin(&id, "config-v1", "generation-a")
        .expect("claim");
    assert!(store.claim_path(&id).exists());

    store
        .fail(&id, "config-v1", "bounded worker failed")
        .expect("terminalize");

    assert!(!store.record_path(&id).exists());
    assert!(!store.claim_path(&id).exists());
    assert!(store.archived_record_path(&id).exists());
    assert_eq!(
        store
            .get(&id)
            .expect("archived receipt")
            .expect("record")
            .receipt
            .status,
        RecoveryStatus::Failed
    );
}

#[test]
fn archived_history_is_not_scanned_but_still_fences_a_spent_head() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let spent = request(HEAD_A, "config-v1");
    store.enqueue(spent.clone()).expect("enqueue spent head");
    store
        .begin(&spent.id, "config-v1", "generation-a")
        .expect("claim spent head");
    store
        .fail(&spent.id, "config-v1", "bounded worker failed")
        .expect("archive spent head");

    // Malformed unrelated cold files prove enqueue does not enumerate the
    // archive. The exact per-head owner remains authoritative for HEAD_A.
    let noise = temp.path().join("archive/noise");
    fs::create_dir_all(&noise).expect("noise directory");
    for index in 0..32 {
        fs::write(noise.join(format!("{index}.json")), b"not-json").expect("cold history noise");
    }
    let mut drifted = spent.clone();
    drifted.failure_fingerprint = "changed-failure".to_owned();
    drifted.id = recovery_id(
        &drifted.repo,
        drifted.pr,
        &drifted.base_ref,
        &drifted.head_sha,
        drifted.merge_queue,
        &drifted.opt_out_label,
        &drifted.failure_fingerprint,
        &drifted.failure_summary,
        &drifted.required_checks,
        &drifted.failure_facts,
        &drifted.policy_signature,
    );
    assert_eq!(
        store.enqueue(drifted).expect("head owner lookup"),
        EnqueueOutcome::HeadAlreadyTracked {
            existing_id: spent.id
        }
    );
    store
        .enqueue(request(HEAD_B, "config-v1"))
        .expect("unrelated head ignores cold archive");
}
