use super::*;

#[test]
fn pending_same_head_drift_is_superseded_and_replaced() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let original = request(HEAD_A, "config-v1");
    store.enqueue(original.clone()).expect("enqueue original");

    let mut changed_evidence = original.clone();
    changed_evidence.failure_summary = "Different normalized evidence.".to_owned();
    changed_evidence.required_checks = required_policy("linux");
    changed_evidence.failure_facts = vec![required_check("linux")];
    changed_evidence.id = recovery_id(
        &changed_evidence.repo,
        changed_evidence.pr,
        &changed_evidence.base_ref,
        &changed_evidence.head_sha,
        changed_evidence.merge_queue,
        &changed_evidence.opt_out_label,
        &changed_evidence.failure_fingerprint,
        &changed_evidence.failure_summary,
        &changed_evidence.required_checks,
        &changed_evidence.failure_facts,
        &changed_evidence.policy_signature,
    );
    let changed_evidence_id = changed_evidence.id.clone();
    assert_eq!(
        store.enqueue(changed_evidence).expect("evidence drift"),
        EnqueueOutcome::Created
    );
    let superseded = store.get(&original.id).expect("load old").expect("old");
    assert_eq!(superseded.receipt.status, RecoveryStatus::Superseded);
    assert_eq!(
        superseded.receipt.superseded_by.as_deref(),
        Some(changed_evidence_id.as_str())
    );

    let mut changed_policy = original.clone();
    changed_policy.policy_signature = "policy-v2".to_owned();
    changed_policy.id = recovery_id(
        &changed_policy.repo,
        changed_policy.pr,
        &changed_policy.base_ref,
        &changed_policy.head_sha,
        changed_policy.merge_queue,
        &changed_policy.opt_out_label,
        &changed_policy.failure_fingerprint,
        &changed_policy.failure_summary,
        &changed_policy.required_checks,
        &changed_policy.failure_facts,
        &changed_policy.policy_signature,
    );
    let changed_policy_id = changed_policy.id.clone();
    assert_eq!(
        store.enqueue(changed_policy).expect("policy drift"),
        EnqueueOutcome::Created
    );
    let superseded = store
        .get(&changed_evidence_id)
        .expect("load evidence")
        .expect("evidence");
    assert_eq!(superseded.receipt.status, RecoveryStatus::Superseded);
    assert_eq!(
        superseded.receipt.superseded_by.as_deref(),
        Some(changed_policy_id.as_str())
    );
    assert_eq!(store.pending(10).expect("pending").len(), 1);
    assert_eq!(
        store
            .enqueue(original.clone())
            .expect("reactivate earlier evidence"),
        EnqueueOutcome::Created
    );
    let current = store
        .get(&changed_policy_id)
        .expect("load changed policy")
        .expect("changed policy");
    assert_eq!(current.receipt.status, RecoveryStatus::Superseded);
    assert_eq!(
        current.receipt.superseded_by.as_deref(),
        Some(original.id.as_str())
    );
    let pending = store.pending(10).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].request.id, original.id);
}

#[test]
fn repeated_same_head_drift_ignores_older_superseded_records() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let original = request(HEAD_A, "config-v1");

    for generation in 0..16 {
        let mut current = original.clone();
        current.failure_summary = format!("normalized failure generation {generation}");
        current.failure_fingerprint = format!("failure-v{generation}");
        current.id = recovery_id(
            &current.repo,
            current.pr,
            &current.base_ref,
            &current.head_sha,
            current.merge_queue,
            &current.opt_out_label,
            &current.failure_fingerprint,
            &current.failure_summary,
            &current.required_checks,
            &current.failure_facts,
            &current.policy_signature,
        );
        assert_eq!(
            store
                .enqueue(current.clone())
                .expect("replace pending drift"),
            EnqueueOutcome::Created
        );
        let pending = store.pending(32).expect("pending records");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request.id, current.id);
    }
}

#[test]
fn changed_worker_config_replaces_only_unattempted_work() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let original = request(HEAD_A, "config-v1");
    let id = original.id.clone();
    store.enqueue(original).expect("enqueue");

    assert_eq!(
        store
            .enqueue(request(HEAD_A, "config-v2"))
            .expect("replace pending config"),
        EnqueueOutcome::Created
    );
    store
        .supersede(&id, None, "trusted config drifted before claim")
        .expect("simulate worker drift reconciliation");
    assert_eq!(
        store
            .enqueue(request(HEAD_A, "config-v3"))
            .expect("reactivate unattempted superseded config"),
        EnqueueOutcome::Created
    );
    let running = store
        .begin(&id, "config-v3", "generation-a")
        .expect("new config claims unattempted request");
    assert_eq!(running.receipt.attempt, 1);
    assert!(matches!(
        store.enqueue(request(HEAD_A, "config-v4")),
        Err(RecoveryError::ConfigDrift { .. })
    ));
    assert_eq!(
        store.get(&id).expect("get").expect("record").receipt.status,
        RecoveryStatus::Running
    );
}

#[test]
fn same_head_drift_supersedes_running_attempt_without_phantom_successor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let original = request(HEAD_A, "config-v1");
    store.enqueue(original.clone()).expect("enqueue");
    store
        .begin(&original.id, "config-v1", "generation-a")
        .expect("claim");

    let mut drifted = original.clone();
    drifted.failure_fingerprint = "failure-v2".to_owned();
    drifted.failure_summary = "A new deterministic failure replaced the running input.".to_owned();
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
        store.enqueue(drifted.clone()).expect("record drift"),
        EnqueueOutcome::HeadAlreadyTracked {
            existing_id: original.id.clone()
        }
    );
    let terminal = store
        .get(&original.id)
        .expect("load")
        .expect("running record");
    assert_eq!(terminal.receipt.status, RecoveryStatus::Superseded);
    assert_eq!(terminal.receipt.attempt, 1);
    assert!(terminal.receipt.superseded_by.is_none());
    assert!(store.get(&drifted.id).expect("load drifted").is_none());
    assert!(store.pending(10).expect("pending").is_empty());
}

#[test]
fn deterministic_clear_supersedes_all_active_exact_target_work() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let pending = request(HEAD_A, "config-v1");
    store.enqueue(pending.clone()).expect("enqueue");

    let superseded = store
        .supersede_active_target(
            &pending.repo,
            pending.pr,
            &pending.head_sha,
            "deterministic recovery cleared",
        )
        .expect("clear target");
    assert_eq!(superseded, vec![pending.id.clone()]);
    assert_eq!(
        store
            .get(&pending.id)
            .expect("load")
            .expect("record")
            .receipt
            .status,
        RecoveryStatus::Superseded
    );
    assert!(store.pending(10).expect("pending").is_empty());
}
