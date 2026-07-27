use chrono::TimeZone;

use super::*;

fn ts(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("timestamp")
}

#[test]
fn parses_and_orders_queue_entries() {
    let body = serde_json::json!({
        "data": {"repository": {"mergeQueue": {"entries": {"nodes": [
            {"position": 1, "enqueuedAt": "2026-07-26T00:00:00Z",
             "headCommit": {"oid": "bbb"}, "pullRequest": {"number": 22}},
            {"position": 0, "enqueuedAt": "2026-07-25T23:00:00Z",
             "headCommit": {"oid": "aaa"}, "pullRequest": {"number": 11}}
        ]}}}}
    });
    let parsed = parse_merge_queue_entries(&body).expect("parse");
    assert_eq!(parsed[0].pr, 11);
    assert_eq!(parsed[0].head_sha.as_deref(), Some("aaa"));
}

#[test]
fn aged_front_without_started_checks_and_with_free_slots_alerts() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("aaa".to_owned()),
        enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];
    let checks = vec![CheckObservation {
        name: "macOS".to_owned(),
        status: "queued".to_owned(),
        started_at: None,
        conclusion: None,
    }];
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &checks,
        active_runs: &[],
        required_contexts: &["macOS".to_owned()],
        eligible_host_classes: &["m1".to_owned(), "m3".to_owned(), "m5".to_owned()],
        routable_free_slots: 2,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert!(report.front_stalled_with_idle_capacity);
    assert_eq!(report.materialized_required_checks, 1);
    assert_eq!(report.progressed_required_checks, 0);
}

#[test]
fn fresh_front_reports_normal_wait_for_queued_or_missing_checks() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("aaa".to_owned()),
        enqueued_at: Some("1970-01-01T00:01:30Z".to_owned()),
        head_observed_at: None,
    }];
    for checks in [
        Vec::new(),
        vec![CheckObservation {
            name: "macOS".to_owned(),
            status: "queued".to_owned(),
            started_at: None,
            conclusion: None,
        }],
    ] {
        let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
            entries: &entries,
            checks: &checks,
            active_runs: &[],
            required_contexts: &["macOS".to_owned()],
            eligible_host_classes: &["m5".to_owned()],
            routable_free_slots: 1,
            stall_threshold_secs: 60,
            now: ts(120),
            enrollment_cleared_prs: &[],
            observation_truncated: false,
        });
        assert!(report.stalled_required_contexts.is_empty());
        assert_eq!(report.reason_codes, [LivenessReason::NormalSerialWait]);
        assert!(!report.needs_attention());
    }
}

#[test]
fn newly_observed_exact_head_gets_its_own_stall_grace() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("new-head".to_owned()),
        enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        head_observed_at: Some("1970-01-01T00:01:30Z".to_owned()),
    }];
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &[],
        active_runs: &[],
        required_contexts: &["macOS".to_owned()],
        eligible_host_classes: &["m5".to_owned()],
        routable_free_slots: 1,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });

    assert_eq!(report.reason_codes, [LivenessReason::NormalSerialWait]);
    assert!(!report.needs_attention());
}

#[test]
fn malformed_head_observation_fails_closed_without_stale_alert() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("new-head".to_owned()),
        enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        head_observed_at: Some("not-a-timestamp".to_owned()),
    }];
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &[],
        active_runs: &[],
        required_contexts: &["macOS".to_owned()],
        eligible_host_classes: &["m5".to_owned()],
        routable_free_slots: 1,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });

    assert!(!report.needs_attention());
}

#[test]
fn aged_front_with_no_configured_or_observed_checks_alerts() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("aaa".to_owned()),
        enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &[],
        active_runs: &[],
        required_contexts: &[],
        eligible_host_classes: &["m5".to_owned()],
        routable_free_slots: 1,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });

    assert_eq!(
        report.stalled_required_contexts,
        ["at-least-one-current-head-check"]
    );
    assert!(report.front_stalled_with_idle_capacity);
    assert!(report.needs_attention());
}

#[test]
fn progress_or_no_idle_capacity_suppresses_front_alert() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("aaa".to_owned()),
        enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];
    let checks = vec![CheckObservation {
        name: "macOS".to_owned(),
        status: "in_progress".to_owned(),
        started_at: Some("1970-01-01T00:01:59Z".to_owned()),
        conclusion: None,
    }];
    let progressed = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &checks,
        active_runs: &[],
        required_contexts: &["macOS".to_owned()],
        eligible_host_classes: &["m5".to_owned()],
        routable_free_slots: 1,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert!(!progressed.front_stalled_with_idle_capacity);
    assert_eq!(progressed.reason_codes, [LivenessReason::NormalSerialWait]);
    let no_capacity = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &[],
        active_runs: &[],
        required_contexts: &["macOS".to_owned()],
        eligible_host_classes: &["m5".to_owned()],
        routable_free_slots: 0,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert!(!no_capacity.front_stalled_with_idle_capacity);
}

#[test]
fn identifies_superseded_and_non_front_capacity_occupiers() {
    let entries = vec![
        MergeQueueEntry {
            pr: 11,
            position: 0,
            head_sha: Some("aaa".to_owned()),
            enqueued_at: None,
            head_observed_at: None,
        },
        MergeQueueEntry {
            pr: 22,
            position: 1,
            head_sha: Some("bbb".to_owned()),
            enqueued_at: None,
            head_observed_at: None,
        },
    ];
    let run = |id, pr, runner: &str| ActiveRunObservation {
        run_id: id,
        workflow: "Build / Test".to_owned(),
        head_branch: format!("gh-readonly-queue/main/pr-{pr}-deadbeef"),
        head_sha: Some(format!("{pr:040x}")),
        status: "in_progress".to_owned(),
        created_at: None,
        pull_requests: vec![pr],
        url: None,
        jobs: vec![JobObservation {
            name: "macOS".to_owned(),
            status: "in_progress".to_owned(),
            runner_name: Some(runner.to_owned()),
            labels: vec!["self-hosted".to_owned()],
        }],
    };
    let active_runs = [run(1, 22, "pulp-m3-01"), run(2, 33, "pulp-m5-01")];
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &[],
        active_runs: &active_runs,
        required_contexts: &[],
        eligible_host_classes: &["m1".to_owned(), "m3".to_owned(), "m5".to_owned()],
        routable_free_slots: 0,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert_eq!(report.capacity_occupiers.len(), 2);
    assert_eq!(report.capacity_occupiers[0].kind, OccupierKind::NonFront);
    assert_eq!(report.capacity_occupiers[1].kind, OccupierKind::Superseded);
    assert!(report.needs_attention());
}

#[test]
fn ignores_hosted_and_unrelated_active_jobs() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: None,
        enqueued_at: None,
        head_observed_at: None,
    }];
    let run = ActiveRunObservation {
        run_id: 9,
        workflow: "Build".to_owned(),
        head_branch: "gh-readonly-queue/main/pr-99-deadbeef".to_owned(),
        head_sha: Some("deadbeef".to_owned()),
        status: "in_progress".to_owned(),
        created_at: None,
        pull_requests: vec![99],
        url: None,
        jobs: vec![JobObservation {
            name: "Linux".to_owned(),
            status: "in_progress".to_owned(),
            runner_name: Some("GitHub Actions 42".to_owned()),
            labels: vec!["ubuntu-latest".to_owned()],
        }],
    };
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &[],
        active_runs: &[run],
        required_contexts: &[],
        eligible_host_classes: &["m1".to_owned(), "m3".to_owned(), "m5".to_owned()],
        routable_free_slots: 1,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert!(report.capacity_occupiers.is_empty());
}

#[test]
fn stale_required_check_and_optional_work_expose_useful_progress_wedge() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("aaa".to_owned()),
        enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];
    let checks = vec![CheckObservation {
        name: "macos".to_owned(),
        status: "in_progress".to_owned(),
        started_at: Some("1970-01-01T00:00:10Z".to_owned()),
        conclusion: None,
    }];
    let optional = ActiveRunObservation {
        run_id: 77,
        workflow: "Examples".to_owned(),
        head_branch: String::new(),
        head_sha: Some("optional".to_owned()),
        status: "in_progress".to_owned(),
        created_at: None,
        pull_requests: vec![99],
        url: None,
        jobs: vec![JobObservation {
            name: "Validate examples (macOS)".to_owned(),
            status: "in_progress".to_owned(),
            runner_name: Some("pulp-vm-m1-01".to_owned()),
            labels: vec!["self-hosted".to_owned()],
        }],
    };
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &checks,
        active_runs: &[optional],
        required_contexts: &["macos".to_owned()],
        eligible_host_classes: &["m1".to_owned()],
        routable_free_slots: 0,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert_eq!(report.stalled_required_contexts, ["macos"]);
    assert!(report.front_blocked_by_capacity_occupiers);
    assert_eq!(
        report.capacity_occupiers[0].kind,
        OccupierKind::OptionalNonQueue
    );
    assert!(
        report
            .reason_codes
            .contains(&LivenessReason::OptionalCapacityTheft)
    );
}

#[test]
fn release_staleness_requires_age_and_unreleased_commits() {
    let stale = assess_release_liveness(
        "v1.0.0".to_owned(),
        "1970-01-01T00:00:00Z".to_owned(),
        3,
        3,
        Some("1.1.0".to_owned()),
        Some(3),
        Some("1970-01-01T00:01:00Z".to_owned()),
        60,
        ts(120),
    )
    .expect("release");
    assert!(stale.stale_with_unreleased_commits);
    assert_eq!(stale.version_unchanged, Some(false));
    let current = assess_release_liveness(
        "v1.0.0".to_owned(),
        "1970-01-01T00:00:00Z".to_owned(),
        0,
        0,
        Some("1.0.0".to_owned()),
        Some(0),
        None,
        60,
        ts(120),
    )
    .expect("release");
    assert!(!current.stale_with_unreleased_commits);
    assert_eq!(current.version_unchanged, Some(true));
}

#[test]
fn required_failure_is_distinct_from_advisory_red() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("aaa".to_owned()),
        enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];
    let checks = vec![
        CheckObservation {
            name: "macos".to_owned(),
            status: "completed".to_owned(),
            started_at: Some("1970-01-01T00:00:10Z".to_owned()),
            conclusion: Some("failure".to_owned()),
        },
        CheckObservation {
            name: "advisory lint".to_owned(),
            status: "completed".to_owned(),
            started_at: Some("1970-01-01T00:00:10Z".to_owned()),
            conclusion: Some("failure".to_owned()),
        },
    ];
    let optional = ActiveRunObservation {
        run_id: 77,
        workflow: "Examples".to_owned(),
        head_branch: String::new(),
        head_sha: Some("optional".to_owned()),
        status: "in_progress".to_owned(),
        created_at: None,
        pull_requests: Vec::new(),
        url: None,
        jobs: vec![JobObservation {
            name: "Validate examples (macOS)".to_owned(),
            status: "in_progress".to_owned(),
            runner_name: Some("pulp-vm-m1-01".to_owned()),
            labels: vec!["self-hosted".to_owned()],
        }],
    };
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &checks,
        active_runs: &[optional],
        required_contexts: &["macos".to_owned()],
        eligible_host_classes: &["m1".to_owned()],
        routable_free_slots: 1,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert_eq!(report.failed_required_contexts, ["macos"]);
    assert!(report.needs_attention());
    assert!(!report.front_stalled_with_idle_capacity);
    assert!(!report.front_blocked_by_capacity_occupiers);
    assert!(
        report
            .reason_codes
            .contains(&LivenessReason::FrontRequiredFailed)
    );
    assert!(
        !report
            .reason_codes
            .contains(&LivenessReason::IdleEligibleCapacity)
    );
    assert!(
        !report
            .reason_codes
            .contains(&LivenessReason::OptionalCapacityTheft)
    );
    assert!(
        !report
            .reason_codes
            .contains(&LivenessReason::NormalSerialWait)
    );
}

#[test]
fn required_stale_check_is_reported_as_terminal_failure() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("aaa".to_owned()),
        enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];
    let checks = vec![CheckObservation {
        name: "macos".to_owned(),
        status: "completed".to_owned(),
        started_at: Some("1970-01-01T00:00:10Z".to_owned()),
        conclusion: Some("stale".to_owned()),
    }];
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &checks,
        active_runs: &[],
        required_contexts: &["macos".to_owned()],
        eligible_host_classes: &["m1".to_owned()],
        routable_free_slots: 1,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });

    assert_eq!(report.failed_required_contexts, ["macos"]);
    assert!(report.needs_attention());
    assert!(
        report
            .reason_codes
            .contains(&LivenessReason::FrontRequiredFailed)
    );
    assert!(
        !report
            .reason_codes
            .contains(&LivenessReason::NormalSerialWait)
    );
}

#[test]
fn observed_failure_is_reported_when_required_context_names_are_unavailable() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("aaa".to_owned()),
        enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];
    let checks = vec![CheckObservation {
        name: "macos".to_owned(),
        status: "completed".to_owned(),
        started_at: Some("1970-01-01T00:00:10Z".to_owned()),
        conclusion: Some("failure".to_owned()),
    }];
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &checks,
        active_runs: &[],
        required_contexts: &[],
        eligible_host_classes: &["m5".to_owned()],
        routable_free_slots: 1,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert_eq!(report.failed_required_contexts, ["macos"]);
    assert!(
        report
            .reason_codes
            .contains(&LivenessReason::FrontRequiredFailed)
    );
}

#[test]
fn observed_success_is_not_a_failure_when_required_context_names_are_unavailable() {
    let checks = vec![CheckObservation {
        name: "macos".to_owned(),
        status: "completed".to_owned(),
        started_at: Some("1970-01-01T00:00:10Z".to_owned()),
        conclusion: Some("success".to_owned()),
    }];
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &[],
        checks: &checks,
        active_runs: &[],
        required_contexts: &[],
        eligible_host_classes: &[],
        routable_free_slots: 0,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert!(report.failed_required_contexts.is_empty());
    assert!(
        !report
            .reason_codes
            .contains(&LivenessReason::FrontRequiredFailed)
    );
}

#[test]
fn newer_success_supersedes_older_failure_when_context_names_are_unavailable() {
    let checks = vec![
        CheckObservation {
            name: "macos".to_owned(),
            status: "completed".to_owned(),
            started_at: Some("1970-01-01T00:00:10Z".to_owned()),
            conclusion: Some("failure".to_owned()),
        },
        CheckObservation {
            name: "macOS".to_owned(),
            status: "completed".to_owned(),
            started_at: Some("1970-01-01T00:00:20Z".to_owned()),
            conclusion: Some("success".to_owned()),
        },
    ];
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &[],
        checks: &checks,
        active_runs: &[],
        required_contexts: &[],
        eligible_host_classes: &[],
        routable_free_slots: 0,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert_eq!(report.materialized_required_checks, 1);
    assert!(report.failed_required_contexts.is_empty());
}

#[test]
fn same_pr_old_merge_group_head_is_superseded_not_front() {
    let entries = vec![MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("new-head".to_owned()),
        enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];
    let old_run = ActiveRunObservation {
        run_id: 77,
        workflow: "Build".to_owned(),
        head_branch: "gh-readonly-queue/main/pr-11-old".to_owned(),
        head_sha: Some("old-head".to_owned()),
        status: "in_progress".to_owned(),
        created_at: None,
        pull_requests: vec![11],
        url: None,
        jobs: vec![JobObservation {
            name: "macOS".to_owned(),
            status: "in_progress".to_owned(),
            runner_name: Some("pulp-vm-m5-01".to_owned()),
            labels: vec!["pulp-build-m5".to_owned()],
        }],
    };
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &entries,
        checks: &[],
        active_runs: &[old_run],
        required_contexts: &["macOS".to_owned()],
        eligible_host_classes: &["m5".to_owned()],
        routable_free_slots: 0,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[],
        observation_truncated: false,
    });
    assert_eq!(report.capacity_occupiers[0].kind, OccupierKind::Superseded);
    assert!(report.needs_attention());
}

#[test]
fn enrollment_loss_is_stable_attention_reason() {
    let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
        entries: &[],
        checks: &[],
        active_runs: &[],
        required_contexts: &[],
        eligible_host_classes: &[],
        routable_free_slots: 0,
        stall_threshold_secs: 60,
        now: ts(120),
        enrollment_cleared_prs: &[11],
        observation_truncated: false,
    });
    assert!(report.needs_attention());
    assert!(
        report
            .reason_codes
            .contains(&LivenessReason::AutoMergeEnrollmentCleared)
    );
    assert!(report.stalled_required_contexts.is_empty());
    assert!(report.reason_codes.contains(&LivenessReason::QueueEmpty));
    assert!(
        !report
            .reason_codes
            .contains(&LivenessReason::FrontRequiredStaleOrMissing)
    );
}

#[test]
fn merge_group_parser_uses_final_pr_marker_for_slash_base() {
    assert_eq!(
        merge_group_pr("gh-readonly-queue/release/pr-preview/pr-42-deadbeef"),
        Some(42)
    );
    assert_eq!(merge_group_pr("topic/pr-42-deadbeef"), None);
}
