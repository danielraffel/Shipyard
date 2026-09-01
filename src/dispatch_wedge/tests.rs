use chrono::TimeZone;

use super::*;
use crate::work_ledger::{
    native_publication_test_policy as policy, native_publication_test_request as request,
};

fn authority() -> DispatchJobAuthority {
    DispatchJobAuthority {
        repository: "owner/repo".to_owned(),
        base_ref: "main".to_owned(),
        pull_request: 43,
        pull_request_head: "a".repeat(40),
        queue_position: 0,
        merge_group_head: "b".repeat(40),
        workflow_run_id: 33_382_580_668,
        workflow_id: 55,
        run_attempt: 3,
        run_event: "merge_group".to_owned(),
        run_head: "b".repeat(40),
        job_id: 99_458_258_698,
        job_name: "macos".to_owned(),
        job_status: "queued".to_owned(),
        job_conclusion: None,
        runner_name: None,
        labels: vec![
            "self-hosted".to_owned(),
            "macOS".to_owned(),
            "ARM64".to_owned(),
            "build".to_owned(),
        ],
        first_observed_unassigned_at: "2026-08-31T10:29:40Z".to_owned(),
        required_context: "macos".to_owned(),
        required_app_id: Some(42),
        producer_app_id: Some(42),
    }
}

fn runner() -> DispatchRunnerObservation {
    DispatchRunnerObservation {
        runner_id: 7,
        name: "m3-slot1".to_owned(),
        status: "online".to_owned(),
        busy: false,
        labels: vec![
            "SELF-HOSTED".to_owned(),
            "macos".to_owned(),
            "arm64".to_owned(),
            "build".to_owned(),
            "extra".to_owned(),
        ],
    }
}

fn assess(
    authority: &DispatchJobAuthority,
    runners: &[DispatchRunnerObservation],
) -> DispatchWedgeAssessment {
    let previous = dispatch_wedge_observation_digest(authority, runners);
    assess_dispatch_wedge(&DispatchWedgeInputs {
        authority,
        runners,
        observation_complete: true,
        previous_observation_digest: Some(&previous),
        assignment_threshold_secs: 900,
        now: Utc
            .with_ymd_and_hms(2026, 8, 31, 13, 35, 0)
            .single()
            .expect("time"),
    })
}

#[test]
fn exact_queued_job_with_registered_compatible_idle_runner_is_dispatch_wedge() {
    let report = assess(&authority(), &[runner()]);
    assert_eq!(report.state, DispatchWedgeState::DispatchWedge);
    let evidence = report.evidence.expect("evidence");
    assert_eq!(evidence.workflow_run_id, 33_382_580_668);
    assert_eq!(evidence.job_id, 99_458_258_698);
    assert_eq!(
        evidence.required_labels_digest,
        required_labels_digest(&normalized_labels(&authority().labels))
    );
    assert_eq!(evidence.eligible_idle_runners.len(), 1);
    assert_eq!(evidence.eligible_idle_runners[0].runner_id, 7);
    assert_eq!(evidence.eligible_idle_runners[0].name, "m3-slot1");
    assert_eq!(evidence.eligible_idle_runners[0].status, "online");
    assert!(!evidence.eligible_idle_runners[0].busy);
    assert_eq!(
        evidence.eligible_idle_runners[0].capacity_basis,
        DispatchCapacityBasis::GitHubRegisteredOnlineIdle
    );
}

#[test]
fn unrelated_runner_churn_does_not_reset_stable_exact_capacity_read() {
    let authority = authority();
    let compatible = runner();
    let first = dispatch_wedge_observation_digest(&authority, std::slice::from_ref(&compatible));
    let unrelated = DispatchRunnerObservation {
        runner_id: 99,
        name: "linux-advisory".to_owned(),
        status: "online".to_owned(),
        busy: false,
        labels: vec!["self-hosted".to_owned(), "linux".to_owned()],
    };
    assert_eq!(
        first,
        dispatch_wedge_observation_digest(&authority, &[compatible, unrelated])
    );
}

#[test]
fn busy_label_mismatch_assignment_and_incomplete_observation_are_negative_controls() {
    let mut busy = runner();
    busy.busy = true;
    assert_eq!(
        assess(&authority(), &[busy]).state,
        DispatchWedgeState::NoCompatibleCapacity
    );
    let mut mismatch = runner();
    mismatch.labels.retain(|label| label != "build");
    assert_eq!(
        assess(&authority(), &[mismatch]).state,
        DispatchWedgeState::NoCompatibleCapacity
    );
    let mut assigned = authority();
    assigned.runner_name = Some("m3-slot1".to_owned());
    assert_eq!(
        assess(&assigned, &[runner()]).state,
        DispatchWedgeState::NotApplicable
    );
    let mut wrong_app = authority();
    wrong_app.producer_app_id = Some(7);
    let app_mismatch = assess(&wrong_app, &[runner()]);
    assert_eq!(app_mismatch.state, DispatchWedgeState::Indeterminate);
    assert_eq!(
        app_mismatch.reason,
        "required_check_app_provenance_mismatch"
    );
    let incomplete = assess_dispatch_wedge(&DispatchWedgeInputs {
        authority: &authority(),
        runners: &[runner()],
        observation_complete: false,
        previous_observation_digest: None,
        assignment_threshold_secs: 1,
        now: Utc::now(),
    });
    assert_eq!(incomplete.state, DispatchWedgeState::Indeterminate);
}

#[test]
fn threshold_and_second_stable_read_prevent_transient_classification() {
    let authority = authority();
    let runners = [runner()];
    let previous = dispatch_wedge_observation_digest(&authority, &runners);
    let fresh = assess_dispatch_wedge(&DispatchWedgeInputs {
        authority: &authority,
        runners: &runners,
        observation_complete: true,
        previous_observation_digest: Some(&previous),
        assignment_threshold_secs: 20_000,
        now: Utc
            .with_ymd_and_hms(2026, 8, 31, 13, 35, 0)
            .single()
            .expect("time"),
    });
    assert_eq!(fresh.state, DispatchWedgeState::Waiting);
    let one_read = assess_dispatch_wedge(&DispatchWedgeInputs {
        authority: &authority,
        runners: &runners,
        observation_complete: true,
        previous_observation_digest: None,
        assignment_threshold_secs: 1,
        now: Utc
            .with_ymd_and_hms(2026, 8, 31, 13, 35, 0)
            .single()
            .expect("time"),
    });
    assert_eq!(one_read.state, DispatchWedgeState::Waiting);
    let mut changed_runner = runner();
    changed_runner.busy = true;
    let changed_snapshot = assess_dispatch_wedge(&DispatchWedgeInputs {
        authority: &authority,
        runners: &[changed_runner],
        observation_complete: true,
        previous_observation_digest: Some(&previous),
        assignment_threshold_secs: 1,
        now: Utc
            .with_ymd_and_hms(2026, 8, 31, 13, 35, 0)
            .single()
            .expect("time"),
    });
    assert_eq!(changed_snapshot.state, DispatchWedgeState::Waiting);
    let invalid_threshold = assess_dispatch_wedge(&DispatchWedgeInputs {
        authority: &authority,
        runners: &runners,
        observation_complete: true,
        previous_observation_digest: Some(&previous),
        assignment_threshold_secs: 0,
        now: Utc
            .with_ymd_and_hms(2026, 8, 31, 13, 35, 0)
            .single()
            .expect("time"),
    });
    assert_eq!(invalid_threshold.state, DispatchWedgeState::Indeterminate);
}

fn published() -> (tempfile::TempDir, DispatchWedgeAssessment) {
    let state = tempfile::tempdir().expect("state");
    let publication = request();
    crate::work_ledger::WorkLedger::open(state.path())
        .expect("ledger")
        .set_repo_policy(
            &crate::work_ledger::RepoPolicy {
                repo: publication.repository.clone(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                revision: 0,
            },
            0,
        )
        .expect("policy");
    crate::work_ledger::WorkLedger::plan_or_apply_native_continuation(
        state.path(),
        &publication,
        &policy(vec![publication.repository.clone()]),
        true,
    )
    .expect("publication");
    let report = assess(&authority(), &[runner()]);
    (state, report)
}

#[test]
fn durable_publication_records_exact_evidence_and_enqueues_once() {
    let (state, report) = published();
    let evidence = report.evidence.as_ref().expect("evidence");
    let ledger = crate::work_ledger::WorkLedger::open_existing(state.path())
        .expect("open")
        .expect("ledger");
    let first = publish_dispatch_wedge(&ledger, None, None, &report)
        .expect("publish")
        .expect("receipt");
    assert!(first.matched);
    assert!(first.wake_enqueued);
    let replay = publish_dispatch_wedge(&ledger, None, None, &report)
        .expect("replay")
        .expect("receipt");
    assert!(replay.matched);
    assert!(!replay.changed);
    assert!(!replay.wake_enqueued);
    assert_eq!(ledger.status().expect("status").pending_wakes, 1);
    let (events, payload): (u64, String) =
        rusqlite::Connection::open(crate::work_ledger::WorkLedger::path_at(state.path()))
            .expect("connection")
            .query_row(
                "SELECT count(*), payload_digest FROM events
              WHERE kind = 'dispatch_wedge_detected'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("durable event");
    assert_eq!(events, 1);
    assert_eq!(payload, evidence.evidence_digest);
    let identity: String =
        rusqlite::Connection::open(crate::work_ledger::WorkLedger::path_at(state.path()))
            .expect("connection")
            .query_row(
                "SELECT payload_digest FROM events WHERE kind = 'dispatch_wedge_identity'",
                [],
                |row| row.get(0),
            )
            .expect("durable identity");
    assert_eq!(identity, evidence.dedupe_key);
}

#[test]
fn exact_identity_changes_produce_distinct_receipts_without_queue_mutation() {
    let first = assess(&authority(), &[runner()]).evidence.expect("first");
    let mut replacement = authority();
    replacement.run_attempt += 1;
    replacement.job_id += 1;
    let second = assess(&replacement, &[runner()]).evidence.expect("second");
    assert_ne!(first.dedupe_key, second.dedupe_key);
}

#[test]
fn retargeted_base_ref_refuses_publication() {
    let (state, mut report) = published();
    let evidence = report.evidence.as_mut().expect("evidence");
    evidence.base_ref = "release".to_owned();
    evidence.dedupe_key = transition_dedupe_key(evidence);
    evidence.evidence_digest = evidence_digest(evidence);
    let ledger = crate::work_ledger::WorkLedger::open_existing(state.path())
        .expect("open")
        .expect("ledger");
    let refused = publish_dispatch_wedge(&ledger, None, None, &report);
    assert!(refused.is_err());
    assert_eq!(ledger.status().expect("status").pending_wakes, 0);
}

#[test]
fn runner_evidence_changes_do_not_duplicate_the_exact_job_transition() {
    let first = assess(&authority(), &[runner()]).evidence.expect("first");
    let mut replacement_runner = runner();
    replacement_runner.name = "m5-slot2".to_owned();
    replacement_runner.runner_id = 8;
    replacement_runner.name = "m3-slot2".to_owned();
    let second = assess(&authority(), &[replacement_runner])
        .evidence
        .expect("second");
    assert_eq!(first.dedupe_key, second.dedupe_key);
    assert_ne!(first.evidence_digest, second.evidence_digest);
}

#[test]
fn queue_position_and_runner_changes_do_not_duplicate_exact_job_transition() {
    let first = assess(&authority(), &[runner()]).evidence.expect("first");
    let mut moved = authority();
    moved.queue_position = 17;
    let mut replacement_runner = runner();
    replacement_runner.name = "m5-slot2".to_owned();
    replacement_runner.runner_id = 8;
    replacement_runner.name = "m3-slot2".to_owned();
    let second = assess(&moved, &[replacement_runner])
        .evidence
        .expect("second");
    assert_eq!(first.dedupe_key, second.dedupe_key);
    assert_ne!(first.evidence_digest, second.evidence_digest);
}
