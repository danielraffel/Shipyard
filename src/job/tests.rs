use serde_json::Value;

use super::{
    CancellationCause, CancellationProof, Job, JobStatus, JobTransitionError, Priority,
    TargetResult, TargetStatus, ValidationMode,
};

fn job() -> Job {
    Job::create(
        "abc123",
        "feat/x",
        vec!["mac".to_owned(), "linux".to_owned()],
        ValidationMode::Full,
        Priority::Normal,
    )
}

#[test]
fn priority_values_match_python_enum() {
    assert_eq!(Priority::Low.value(), 10);
    assert_eq!(Priority::Normal.value(), 50);
    assert_eq!(Priority::High.value(), 100);
}

#[test]
fn target_status_terminal_matches_python_contract() {
    assert!(!TargetStatus::Pending.is_terminal());
    assert!(!TargetStatus::Running.is_terminal());
    for status in [
        TargetStatus::Pass,
        TargetStatus::Fail,
        TargetStatus::Error,
        TargetStatus::Unreachable,
        TargetStatus::Cancelled,
    ] {
        assert!(status.is_terminal(), "{status:?}");
    }
}

#[test]
fn target_result_serializes_python_shape() {
    let mut result = TargetResult::new("mac", "macos", TargetStatus::Pass, "local");
    result.duration_secs = Some(1.24);
    result.contract_markers_seen = vec!["SMOKE".to_owned()];
    let value = result.to_json_value();
    assert_eq!(value["target"], "mac");
    assert_eq!(value["platform"], "macos");
    assert_eq!(value["status"], "pass");
    assert_eq!(value["backend"], "local");
    assert_eq!(value["contract_markers_seen"][0], "SMOKE");
    assert!(value.get("error_message").is_none());
}

#[test]
fn job_create_sets_pending_state_and_id_shape() {
    let job = job();
    assert!(job.id.starts_with("sy-"));
    assert_eq!(job.status, JobStatus::Pending);
    assert_eq!(job.mode, ValidationMode::Full);
    assert_eq!(job.priority, Priority::Normal);
    assert_eq!(job.target_names, vec!["mac", "linux"]);
}

fn running_with_heartbeat(
    now: chrono::DateTime<chrono::Utc>,
    started_offset_secs: i64,
    heartbeat_offset_secs: Option<i64>,
) -> Job {
    let mut running = job().start().expect("pending job starts");
    running.started_at = Some(now - chrono::Duration::seconds(started_offset_secs));
    if let Some(offset) = heartbeat_offset_secs {
        let mut result = TargetResult::new("mac", "macos", TargetStatus::Running, "local");
        result.last_heartbeat_at = Some(now - chrono::Duration::seconds(offset));
        running.results.insert("mac".to_owned(), result);
    }
    running
}

#[test]
fn last_liveness_at_prefers_newest_heartbeat_then_started_at() {
    let now = chrono::Utc::now();
    let mut running = running_with_heartbeat(now, 600, None);
    // No heartbeats yet -> falls back to started_at.
    assert_eq!(running.last_liveness_at(), running.started_at);

    let mut older = TargetResult::new("mac", "macos", TargetStatus::Running, "local");
    older.last_heartbeat_at = Some(now - chrono::Duration::seconds(300));
    let mut newer = TargetResult::new("linux", "linux", TargetStatus::Running, "local");
    newer.last_heartbeat_at = Some(now - chrono::Duration::seconds(30));
    running.results.insert("mac".to_owned(), older);
    running.results.insert("linux".to_owned(), newer);
    // Freshest heartbeat wins over started_at and the older heartbeat.
    assert_eq!(
        running.last_liveness_at(),
        Some(now - chrono::Duration::seconds(30))
    );
}

#[test]
fn last_liveness_at_includes_started_at_over_stale_retained_heartbeat() {
    let now = chrono::Utc::now();
    // A requeued-then-restarted job: a fresh `started_at` but an old
    // terminal-result heartbeat retained from the prior run. Liveness is the
    // newer `started_at`, so the restarted live job is not stale.
    let restarted = running_with_heartbeat(now, 5, Some(1000));
    assert_eq!(restarted.last_liveness_at(), restarted.started_at);
    assert!(!restarted.is_stale_running(now, chrono::Duration::seconds(180)));
}

#[test]
fn is_stale_running_only_for_running_jobs() {
    let now = chrono::Utc::now();
    let stale_after = chrono::Duration::seconds(180);
    // Pending job: never stale, even though created_at is "now".
    assert!(!job().is_stale_running(now, stale_after));
    // Cancelled job: never stale.
    let cancelled = job().cancel().expect("pending job cancels");
    assert!(!cancelled.is_stale_running(now, stale_after));
    // Completed job: never stale.
    let completed = job()
        .start()
        .and_then(|running| running.complete())
        .expect("pending -> running -> completed");
    assert!(!completed.is_stale_running(now, stale_after));
}

#[test]
fn is_stale_running_uses_heartbeat_age_threshold() {
    let now = chrono::Utc::now();
    let stale_after = chrono::Duration::seconds(180);
    // Ancient started_at but a fresh heartbeat -> not stale.
    let fresh = running_with_heartbeat(now, 1000, Some(30));
    assert!(!fresh.is_stale_running(now, stale_after));
    // Just under the threshold -> not stale.
    let almost = running_with_heartbeat(now, 1000, Some(179));
    assert!(!almost.is_stale_running(now, stale_after));
    // At/over the threshold -> stale.
    let stale = running_with_heartbeat(now, 1000, Some(200));
    assert!(stale.is_stale_running(now, stale_after));
}

#[test]
fn is_stale_running_uses_started_at_when_no_heartbeat() {
    let now = chrono::Utc::now();
    let stale_after = chrono::Duration::seconds(180);
    // Running, never heartbeat, started long ago -> stale via started_at.
    let stale = running_with_heartbeat(now, 1000, None);
    assert!(stale.is_stale_running(now, stale_after));
    // Running, never heartbeat, started recently -> not stale.
    let fresh = running_with_heartbeat(now, 5, None);
    assert!(!fresh.is_stale_running(now, stale_after));
}

#[test]
fn is_stale_running_handles_no_anchor_skew_and_zero_threshold() {
    let now = chrono::Utc::now();
    let stale_after = chrono::Duration::seconds(180);
    // No anchor at all (no started_at, no heartbeat) -> not stale.
    let mut no_anchor = running_with_heartbeat(now, 1000, None);
    no_anchor.started_at = None;
    assert!(!no_anchor.is_stale_running(now, stale_after));
    // Future-dated heartbeat (clock skew) -> not stale.
    let skewed = running_with_heartbeat(now, 0, Some(-60));
    assert!(!skewed.is_stale_running(now, stale_after));
    // Zero/negative threshold -> never stale.
    let ancient = running_with_heartbeat(now, 10_000, Some(10_000));
    assert!(!ancient.is_stale_running(now, chrono::Duration::zero()));
}

#[test]
fn job_transitions_are_immutable() {
    let pending = job();
    let running = pending.start().expect("start");
    assert_eq!(pending.status, JobStatus::Pending);
    assert_eq!(running.status, JobStatus::Running);
    assert!(running.started_at.is_some());

    let completed = running.complete().expect("complete");
    assert_eq!(completed.status, JobStatus::Completed);
    assert!(completed.completed_at.is_some());
}

#[test]
fn invalid_transitions_return_errors() {
    let pending = job();
    assert_eq!(
        pending.complete().expect_err("cannot complete pending"),
        JobTransitionError::InvalidComplete(JobStatus::Pending)
    );

    let completed = pending
        .start()
        .expect("start")
        .complete()
        .expect("complete");
    assert_eq!(
        completed.start().expect_err("cannot restart completed"),
        JobTransitionError::InvalidStart(JobStatus::Completed)
    );
    assert_eq!(
        completed.cancel().expect_err("cannot cancel completed"),
        JobTransitionError::InvalidCancel(JobStatus::Completed)
    );
}

#[test]
fn cancel_sets_terminal_cancelled_state() {
    let cancelled = job().cancel().expect("cancel");
    assert_eq!(cancelled.status, JobStatus::Cancelled);
    assert!(cancelled.completed_at.is_some());
    assert!(cancelled.cancel_requested_at.is_some());
}

#[test]
fn repeated_running_cancel_preserves_first_request_and_authenticated_proof() {
    let running = job().start().expect("start");
    let proof = CancellationProof {
        cause: CancellationCause::AlreadyMerged,
        repository: "owner/repo".to_owned(),
        pull_request: 42,
        head_sha: running.sha.clone(),
    };
    let first = running
        .request_cancel_with_reason_and_proof(
            Some("authenticated merge observation".to_owned()),
            Some(proof.clone()),
        )
        .expect("first request");
    let first_requested_at = first.cancel_requested_at;
    let repeated = first
        .request_cancel_with_reason(Some("operator retry".to_owned()))
        .expect("repeated request");

    assert_eq!(repeated.cancel_requested_at, first_requested_at);
    assert_eq!(repeated.cancellation_proof, Some(proof));
    assert_eq!(
        repeated.cancellation_reason.as_deref(),
        Some("authenticated merge observation")
    );

    let contradictory = CancellationProof {
        cause: CancellationCause::AlreadyMerged,
        repository: "owner/repo".to_owned(),
        pull_request: 43,
        head_sha: running.sha,
    };
    assert_eq!(
        repeated
            .request_cancel_with_reason_and_proof(None, Some(contradictory))
            .expect_err("contradictory authority must refuse"),
        JobTransitionError::ConflictingCancellationProof
    );
}

#[test]
fn with_priority_and_result_return_updated_copies() {
    let job = job();
    let high = job.with_priority(Priority::High);
    assert_eq!(job.priority, Priority::Normal);
    assert_eq!(high.priority, Priority::High);

    let result = TargetResult::new("mac", "macos", TargetStatus::Pass, "local");
    let updated = job.with_result(result);
    assert!(job.results.is_empty());
    assert_eq!(updated.results["mac"].status, TargetStatus::Pass);
}

#[test]
fn passed_requires_completed_and_all_targets_passed() {
    let running = job().start().expect("start");
    let with_mac = running.with_result(TargetResult::new(
        "mac",
        "macos",
        TargetStatus::Pass,
        "local",
    ));
    assert!(!with_mac.passed());
    assert!(!with_mac.all_targets_terminal());

    let with_linux = with_mac.with_result(TargetResult::new(
        "linux",
        "linux",
        TargetStatus::Pass,
        "ssh",
    ));
    assert!(with_linux.all_targets_terminal());
    assert!(!with_linux.passed());

    let completed = with_linux.complete().expect("complete");
    assert!(completed.passed());
}

#[test]
fn failed_target_is_terminal_but_not_passed() {
    let running = job().start().expect("start");
    let with_results = running
        .with_result(TargetResult::new(
            "mac",
            "macos",
            TargetStatus::Pass,
            "local",
        ))
        .with_result(TargetResult::new(
            "linux",
            "linux",
            TargetStatus::Fail,
            "ssh",
        ));
    assert!(with_results.all_targets_terminal());
    assert!(!with_results.complete().expect("complete").passed());
}

#[test]
fn job_serializes_status_and_results() {
    let running = job().start().expect("start").with_result(TargetResult::new(
        "mac",
        "macos",
        TargetStatus::Pass,
        "local",
    ));
    let value = running.to_json_value();
    assert_eq!(value["status"], "running");
    assert_eq!(value["overall"], "running");
    assert_eq!(value["priority"], "normal");
    assert_eq!(value["results"]["mac"]["status"], "pass");
    assert_eq!(
        value["targets"],
        Value::Array(vec!["mac".into(), "linux".into()])
    );
}

#[test]
fn legacy_job_without_kind_deserializes() {
    let value = serde_json::json!({
        "id": "sy-legacy",
        "sha": "abc123",
        "branch": "feat/x",
        "mode": "full",
        "targets": ["mac"],
        "priority": "normal",
        "status": "pending",
        "created_at": "2026-05-26T00:00:00Z"
    });

    let job: Job = serde_json::from_value(value).expect("legacy job");

    assert_eq!(job.kind, None);
    assert_eq!(job.workload_scope, None);
    assert!(job.resource_claims.is_empty());
    assert_eq!(job.cancel_requested_at, None);
}
