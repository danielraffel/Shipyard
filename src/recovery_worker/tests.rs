use super::*;

const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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

fn request(head: &str, config: &str) -> RecoveryRequest {
    RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        head,
        "failure-v1",
        "Required Windows check failed at the exact head.",
        required_policy("windows"),
        vec![required_check("windows")],
        "policy-v1",
        config,
    )
    .expect("request")
}

fn no_change_output() -> RecoveryOutput {
    RecoveryOutput {
        schema_version: RECOVERY_SCHEMA_VERSION,
        verdict: RecoveryVerdict::NoChange,
        category: RecoveryCategory::Compile,
        confidence: RecoveryConfidence::High,
        evidence: vec![],
        candidate_paths: vec![],
        focused_tests: vec![],
    }
}

fn escalation_output() -> RecoveryOutput {
    RecoveryOutput {
        verdict: RecoveryVerdict::Escalate,
        ..no_change_output()
    }
}

#[test]
fn deterministic_identity_deduplicates_the_exact_tuple() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let first = request(HEAD_A, "config-v1");
    let duplicate = request(HEAD_A, "config-v1");

    assert_eq!(first.id, duplicate.id);
    assert_eq!(
        store.enqueue(first.clone()).expect("enqueue"),
        EnqueueOutcome::Created
    );
    assert_eq!(
        store.enqueue(duplicate).expect("dedupe"),
        EnqueueOutcome::Existing
    );
    let persisted = store.get(&first.id).expect("get").expect("record");
    assert_eq!(persisted.receipt.status, RecoveryStatus::Pending);
    assert_eq!(persisted.receipt.max_attempts, 1);
}

#[test]
fn new_exact_head_produces_a_new_request_id() {
    let first = request(HEAD_A, "config-v1");
    let second = request(HEAD_B, "config-v1");

    assert_ne!(first.id, second.id);
}

#[test]
fn configured_opt_out_label_is_bound_to_request_identity() {
    let original = request(HEAD_A, "config-v1");
    let custom = RecoveryRequest::new_with_steward_policy(
        &original.repo,
        original.pr,
        &original.base_ref,
        &original.head_sha,
        false,
        "custom:no-recovery",
        &original.failure_fingerprint,
        &original.failure_summary,
        original.required_checks.clone(),
        original.failure_facts.clone(),
        &original.policy_signature,
        &original.config_signature,
    )
    .expect("custom opt-out policy");
    assert_ne!(original.id, custom.id);

    let merge_queue = RecoveryRequest::new_with_steward_policy(
        &original.repo,
        original.pr,
        &original.base_ref,
        &original.head_sha,
        true,
        &original.opt_out_label,
        &original.failure_fingerprint,
        &original.failure_summary,
        original.required_checks.clone(),
        original.failure_facts.clone(),
        &original.policy_signature,
        &original.config_signature,
    )
    .expect("merge-queue policy");
    assert_ne!(original.id, merge_queue.id);

    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let mut tampered = custom;
    tampered.opt_out_label = "different:no-recovery".to_owned();
    assert!(matches!(
        store.enqueue(tampered),
        Err(RecoveryError::IdentityCollision(_))
    ));
}

#[test]
fn retargeted_base_produces_a_new_request_id() {
    let original = request(HEAD_A, "config-v1");
    let retargeted = RecoveryRequest::new(
        &original.repo,
        original.pr,
        "release",
        &original.head_sha,
        &original.failure_fingerprint,
        &original.failure_summary,
        original.required_checks.clone(),
        original.failure_facts.clone(),
        &original.policy_signature,
        &original.config_signature,
    )
    .expect("retargeted request");

    assert_ne!(original.id, retargeted.id);
}

#[test]
fn changed_normalized_failure_evidence_produces_a_new_request_id() {
    let original = request(HEAD_A, "config-v1");
    let mut changed_summary = original.clone();
    changed_summary.failure_summary = "A different normalized failure.".to_owned();
    changed_summary.id = recovery_id(
        &changed_summary.repo,
        changed_summary.pr,
        &changed_summary.base_ref,
        &changed_summary.head_sha,
        changed_summary.merge_queue,
        &changed_summary.opt_out_label,
        &changed_summary.failure_fingerprint,
        &changed_summary.failure_summary,
        &changed_summary.required_checks,
        &changed_summary.failure_facts,
        &changed_summary.policy_signature,
    );
    let mut changed_context = original.clone();
    changed_context.required_checks = required_policy("linux");
    changed_context.failure_facts = vec![required_check("linux")];
    changed_context.id = recovery_id(
        &changed_context.repo,
        changed_context.pr,
        &changed_context.base_ref,
        &changed_context.head_sha,
        changed_context.merge_queue,
        &changed_context.opt_out_label,
        &changed_context.failure_fingerprint,
        &changed_context.failure_summary,
        &changed_context.required_checks,
        &changed_context.failure_facts,
        &changed_context.policy_signature,
    );

    assert_ne!(original.id, changed_summary.id);
    assert_ne!(original.id, changed_context.id);

    let expanded_policy = RecoveryRequest::new(
        &original.repo,
        original.pr,
        &original.base_ref,
        &original.head_sha,
        &original.failure_fingerprint,
        &original.failure_summary,
        vec![
            RecoveryRequiredCheck {
                context: "signing".to_owned(),
                app_id: Some(7),
            },
            RecoveryRequiredCheck {
                context: "windows".to_owned(),
                app_id: None,
            },
        ],
        original.failure_facts.clone(),
        &original.policy_signature,
        &original.config_signature,
    )
    .expect("expanded policy snapshot");
    assert_ne!(original.id, expanded_policy.id);
}

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
fn request_rejects_missing_oversize_or_nul_failure_facts() {
    let make = |summary: String, contexts: Vec<RecoveryFailureFact>| {
        RecoveryRequest::new(
            "Generous-Corp/pulp",
            42,
            "main",
            HEAD_A,
            "failure-v1",
            summary,
            contexts
                .iter()
                .filter_map(|fact| match fact {
                    RecoveryFailureFact::RequiredCheck {
                        context, app_id, ..
                    } => Some(RecoveryRequiredCheck {
                        context: context.clone(),
                        app_id: *app_id,
                    }),
                    RecoveryFailureFact::MergeState { .. } => None,
                })
                .collect(),
            contexts,
            "policy-v1",
            "config-v1",
        )
    };

    assert!(matches!(
        make("summary".to_owned(), Vec::new()),
        Err(RecoveryError::InvalidRequest(_))
    ));
    assert!(matches!(
        RecoveryRequest::new(
            "Generous-Corp/pulp",
            42,
            "main",
            HEAD_A,
            "failure-v1",
            "summary",
            Vec::new(),
            vec![required_check("check")],
            "policy-v1",
            "config-v1",
        ),
        Err(RecoveryError::InvalidRequest(_))
    ));
    assert!(matches!(
        make(
            "x".repeat(MAX_FAILURE_SUMMARY_BYTES + 1),
            vec![required_check("check")]
        ),
        Err(RecoveryError::InvalidRequest(_))
    ));
    assert!(matches!(
        make(
            "summary".to_owned(),
            vec![required_check("check\0injection")]
        ),
        Err(RecoveryError::InvalidRequest(_))
    ));
    assert!(matches!(
        make(
            "summary".to_owned(),
            vec![required_check("check"); MAX_FAILED_CONTEXTS + 1]
        ),
        Err(RecoveryError::InvalidRequest(_))
    ));
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

#[test]
fn validated_escalation_completes_to_the_expected_terminal_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let request = request(HEAD_A, "config-v1");
    let id = request.id.clone();
    store.enqueue(request).expect("enqueue");
    store
        .begin(&id, "config-v1", "generation-a")
        .expect("begin");

    let terminal = store
        .complete(&id, "config-v1", escalation_output())
        .expect("complete");
    assert_eq!(terminal.receipt.status, RecoveryStatus::Escalated);
    assert_eq!(
        terminal.receipt.output.as_ref().expect("output").verdict,
        RecoveryVerdict::Escalate
    );
}

#[test]
fn output_validation_rejects_unsafe_shapes() {
    let mut output = escalation_output();
    output.candidate_paths = vec!["../Cargo.toml".to_owned()];
    assert!(matches!(
        output.validate(),
        Err(RecoveryError::InvalidOutput(_))
    ));
}

#[test]
fn output_schema_requires_all_reserved_empty_arrays() {
    for missing in ["evidence", "candidate_paths", "focused_tests"] {
        let mut value = serde_json::json!({
            "schema_version": RECOVERY_SCHEMA_VERSION,
            "verdict": "no_change",
            "category": "compile",
            "confidence": "high",
            "evidence": [],
            "candidate_paths": [],
            "focused_tests": []
        });
        value
            .as_object_mut()
            .expect("output object")
            .remove(missing);
        assert!(
            serde_json::from_value::<RecoveryOutput>(value).is_err(),
            "missing `{missing}` must violate the strict schema"
        );
    }
}

#[test]
fn output_validation_enforces_phase_one_boundary() {
    let mut too_many_evidence = escalation_output();
    too_many_evidence.evidence = vec!["untrusted claim".to_owned()];
    assert!(matches!(
        too_many_evidence.validate(),
        Err(RecoveryError::InvalidOutput(_))
    ));

    let mut too_many_paths = escalation_output();
    too_many_paths.candidate_paths = vec!["src/example.rs".to_owned()];
    assert!(matches!(
        too_many_paths.validate(),
        Err(RecoveryError::InvalidOutput(_))
    ));

    let mut too_many_tests = escalation_output();
    too_many_tests.focused_tests = vec!["cargo test".to_owned()];
    assert!(matches!(
        too_many_tests.validate(),
        Err(RecoveryError::InvalidOutput(_))
    ));

    let prose = serde_json::json!({
        "schema_version": RECOVERY_SCHEMA_VERSION,
        "verdict": "escalate",
        "category": "unknown",
        "confidence": "low",
        "summary": "The compiler failed because a source file is malformed.",
        "evidence": [],
        "candidate_paths": [],
        "focused_tests": []
    });
    assert!(serde_json::from_value::<RecoveryOutput>(prose).is_err());
}

#[test]
fn phase_one_forbids_repair_and_no_change_but_accepts_low_confidence_escalation() {
    let mut repair = escalation_output();
    repair.verdict = RecoveryVerdict::BoundedRepair;
    assert!(matches!(
        repair.validate(),
        Err(RecoveryError::InvalidOutput(_))
    ));

    let no_change = no_change_output();
    assert!(matches!(
        no_change.validate(),
        Err(RecoveryError::InvalidOutput(_))
    ));

    let mut low = escalation_output();
    low.confidence = RecoveryConfidence::Low;
    low.validate().expect("low confidence escalates");
}

#[test]
fn phase_one_cannot_downgrade_any_request_by_category_or_confidence() {
    let request = request(HEAD_A, "config-v1");
    for category in [
        RecoveryCategory::Compile,
        RecoveryCategory::Test,
        RecoveryCategory::Conflict,
        RecoveryCategory::Security,
        RecoveryCategory::Workflow,
        RecoveryCategory::Infra,
        RecoveryCategory::Unknown,
    ] {
        let mut output = no_change_output();
        output.category = category;
        output.confidence = RecoveryConfidence::High;
        assert!(
            matches!(
                output.validate_for_request(&request),
                Err(RecoveryError::InvalidOutput(_))
            ),
            "category {category:?} cannot suppress escalation"
        );
    }

    escalation_output()
        .validate_for_request(&request)
        .expect("explicit escalation satisfies phase-1 routing policy");
}

#[test]
fn durable_completion_rechecks_request_aware_escalation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let mut request = request(HEAD_A, "config-v1");
    request.failure_facts = vec![RecoveryFailureFact::MergeState {
        state: "DIRTY".to_owned(),
    }];
    request.id = recovery_id(
        &request.repo,
        request.pr,
        &request.base_ref,
        &request.head_sha,
        request.merge_queue,
        &request.opt_out_label,
        &request.failure_fingerprint,
        &request.failure_summary,
        &request.required_checks,
        &request.failure_facts,
        &request.policy_signature,
    );
    let id = request.id.clone();
    store.enqueue(request).expect("enqueue");
    store
        .begin(&id, "config-v1", "generation-a")
        .expect("begin");
    assert!(matches!(
        store.complete(&id, "config-v1", no_change_output()),
        Err(RecoveryError::InvalidOutput(_))
    ));
    assert_eq!(
        store
            .get(&id)
            .expect("load")
            .expect("record")
            .receipt
            .status,
        RecoveryStatus::Running
    );
}

#[test]
fn malformed_or_drifted_durable_state_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = RecoveryStore::new(temp.path()).expect("store");
    let request = request(HEAD_A, "config-v1");
    let id = request.id.clone();
    store.enqueue(request).expect("enqueue");

    let path = store.record_path(&id);
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
    value["receipt"]["config_signature"] = serde_json::json!("config-v2");
    fs::write(&path, serde_json::to_vec(&value).expect("encode")).expect("write");

    assert!(matches!(
        store.get(&id),
        Err(RecoveryError::ConfigDrift { .. })
    ));
}
