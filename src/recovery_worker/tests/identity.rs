use super::*;

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
