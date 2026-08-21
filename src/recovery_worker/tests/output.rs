use super::*;

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
