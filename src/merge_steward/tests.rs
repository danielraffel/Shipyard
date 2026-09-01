use super::*;

fn sha(byte: char) -> String {
    std::iter::repeat_n(byte, 40).collect()
}

fn green_pr() -> StewardPullRequest {
    StewardPullRequest {
        number: 7,
        head_sha: sha('a'),
        head_branch: "feature".to_owned(),
        draft: false,
        merge_state: "CLEAN".to_owned(),
        auto_merge_active: false,
        queue_position: None,
        labels: Vec::new(),
        checks: vec![StewardCheck {
            name: "required".to_owned(),
            source: StewardCheckSource::CheckRun,
            app_id: None,
            status: "COMPLETED".to_owned(),
            conclusion: Some("SUCCESS".to_owned()),
            run_id: Some(10),
            observed_at: Some("2026-07-26T00:00:00Z".to_owned()),
        }],
    }
}

fn queue_policy() -> StewardPolicy {
    StewardPolicy {
        merge_queue: true,
        native_auto_merge: true,
        required_checks: vec![RequiredCheck {
            context: "required".to_owned(),
            app_id: None,
        }],
        opt_out_label: "shipyard:no-auto-merge".to_owned(),
        provenance_blocking_labels: vec!["5·unresolved".to_owned()],
        managed_label: None,
        handoff_context: "shipyard/steward-handoff".to_owned(),
        max_transient_reruns: 1,
    }
}

#[test]
fn explicit_management_requires_label_and_current_head_receipt() {
    let mut policy = queue_policy();
    policy.managed_label = Some("shipyard:managed".to_owned());
    let mut pr = green_pr();
    assert_eq!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::Unmanaged
    );

    pr.labels.push("shipyard:managed".to_owned());
    assert_eq!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::HandoffMissing
    );

    pr.checks.push(StewardCheck {
        name: "shipyard/steward-handoff".to_owned(),
        source: StewardCheckSource::StatusContext,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: None,
        observed_at: Some("2026-08-13T00:00:00Z".to_owned()),
    });
    assert_eq!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::ArmMergeQueue
    );
}

#[test]
fn check_run_cannot_impersonate_handoff_status_context() {
    let mut policy = queue_policy();
    policy.managed_label = Some("shipyard:managed".to_owned());
    let mut pr = green_pr();
    pr.labels.push("shipyard:managed".to_owned());
    pr.checks.push(StewardCheck {
        name: "shipyard/steward-handoff".to_owned(),
        source: StewardCheckSource::CheckRun,
        app_id: Some(1),
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: Some(99),
        observed_at: Some("2026-08-13T00:00:00Z".to_owned()),
    });
    assert_eq!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::HandoffMissing
    );
}

#[test]
fn queued_entry_is_authority_even_when_auto_merge_request_is_null() {
    let mut pr = green_pr();
    pr.queue_position = Some(11);
    assert_eq!(
        classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
        StewardDecision::Queued { position: 11 }
    );
}

#[test]
fn provenance_blocker_is_case_insensitive_and_precedes_queue_authority() {
    let mut pr = green_pr();
    pr.queue_position = Some(11);
    pr.labels.push("5·UnReSoLvEd".to_owned());
    let decision = classify_pr(&pr, &queue_policy(), &BTreeMap::new());
    assert_eq!(
        decision,
        StewardDecision::ProvenanceBlocked {
            labels: vec!["5·unresolved".to_owned()],
        }
    );
    assert_eq!(
        serde_json::to_value(&decision).expect("serialize"),
        serde_json::json!({
            "action": "provenance_blocked",
            "labels": ["5·unresolved"]
        })
    );
}

#[test]
fn provenance_blocker_precedes_opt_out_with_exact_json() {
    let mut pr = green_pr();
    pr.labels
        .extend(["STEWARD:SKIP".to_owned(), "5·Unresolved".to_owned()]);
    let decision = classify_pr(&pr, &queue_policy(), &BTreeMap::new());
    assert_eq!(
        serde_json::to_value(decision).expect("serialize"),
        serde_json::json!({
            "action": "provenance_blocked",
            "labels": ["5·unresolved"]
        })
    );
}

#[test]
fn only_a_current_observation_without_the_blocker_regains_authority() {
    let policy = queue_policy();
    let mut pr = green_pr();
    pr.labels.push("5·unresolved".to_owned());
    assert!(matches!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::ProvenanceBlocked { .. }
    ));

    pr.labels.clear();
    assert_eq!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::ArmMergeQueue
    );

    pr.head_sha = sha('b');
    pr.labels.push("5·UNRESOLVED".to_owned());
    assert!(matches!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::ProvenanceBlocked { .. }
    ));
}

#[test]
fn ignores_advisory_failure_when_required_context_is_green() {
    let mut pr = green_pr();
    pr.checks.push(StewardCheck {
        name: "advisory".to_owned(),
        source: StewardCheckSource::CheckRun,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("FAILURE".to_owned()),
        run_id: Some(11),
        observed_at: Some("2026-07-26T00:00:00Z".to_owned()),
    });
    assert_eq!(
        classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
        StewardDecision::ArmMergeQueue
    );
}

#[test]
fn newest_duplicate_required_context_is_authoritative() {
    let mut pr = green_pr();
    pr.checks[0].conclusion = Some("FAILURE".to_owned());
    pr.checks[0].observed_at = Some("2026-07-25T00:00:00Z".to_owned());
    pr.checks.push(StewardCheck {
        name: "required".to_owned(),
        source: StewardCheckSource::CheckRun,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: Some(12),
        observed_at: Some("2026-07-26T00:00:00Z".to_owned()),
    });
    assert_eq!(
        classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
        StewardDecision::ArmMergeQueue
    );
}

#[test]
fn undated_pending_duplicate_blocks_older_success() {
    let mut pr = green_pr();
    pr.checks[0].observed_at = Some("2026-07-26T00:00:00Z".to_owned());
    pr.checks.push(StewardCheck {
        name: "required".to_owned(),
        source: StewardCheckSource::CheckRun,
        app_id: None,
        status: "QUEUED".to_owned(),
        conclusion: None,
        run_id: Some(12),
        observed_at: None,
    });
    assert!(matches!(
        classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
        StewardDecision::WaitingRequired { .. }
    ));
}

#[test]
fn app_bound_requirement_accepts_only_the_matching_check_producer() {
    let mut policy = queue_policy();
    policy.required_checks[0].app_id = Some(42);
    let mut pr = green_pr();
    pr.checks[0].app_id = Some(42);
    assert_eq!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::ArmMergeQueue
    );

    pr.checks[0].app_id = Some(7);
    assert!(matches!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::WaitingRequired { contexts }
            if contexts == vec!["required (app_id=42)"]
    ));
}

#[test]
fn app_bound_requirement_fails_closed_when_producer_identity_is_unavailable() {
    let mut policy = queue_policy();
    policy.required_checks[0].app_id = Some(42);
    let pr = green_pr();
    assert!(matches!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::WaitingRequired { contexts }
            if contexts == vec!["required (app_id=42)"]
    ));
}

#[test]
fn direct_merge_refuses_partially_materialized_checks_without_authoritative_policy() {
    let mut policy = queue_policy();
    policy.merge_queue = false;
    policy.native_auto_merge = false;
    policy.required_checks.clear();
    assert_eq!(
        classify_pr(&green_pr(), &policy, &BTreeMap::new()),
        StewardDecision::DirectMergeRefused {
            reasons: vec![
                DirectMergeRefusal::RequiredCheckMaterializationNotAuthoritative,
                DirectMergeRefusal::ValidatedBaseRevisionNotAtomic,
            ]
        }
    );
    let mut red = green_pr();
    red.checks[0].conclusion = Some("FAILURE".to_owned());
    assert_eq!(
        classify_pr(&red, &policy, &BTreeMap::new()),
        StewardDecision::DirectMergeRefused {
            reasons: vec![
                DirectMergeRefusal::RequiredCheckMaterializationNotAuthoritative,
                DirectMergeRefusal::ValidatedBaseRevisionNotAtomic,
            ]
        }
    );
}

#[test]
fn merge_queue_refuses_partially_materialized_checks_without_authoritative_policy() {
    let mut policy = queue_policy();
    policy.required_checks.clear();
    let expected = StewardDecision::WaitingRequired {
        contexts: vec!["authoritative-required-check-policy".to_owned()],
    };
    let mut no_checks = green_pr();
    no_checks.checks.clear();
    let green = green_pr();
    let mut failed = green_pr();
    failed.checks[0].conclusion = Some("FAILURE".to_owned());
    let mut transient = green_pr();
    transient.checks[0].conclusion = Some("TIMED_OUT".to_owned());
    for pr in [no_checks, green, failed, transient] {
        assert_eq!(classify_pr(&pr, &policy, &BTreeMap::new()), expected);
    }
}

#[test]
fn direct_merge_refuses_unvalidated_base_advance_even_with_authoritative_checks() {
    let mut policy = queue_policy();
    policy.merge_queue = false;
    policy.native_auto_merge = false;
    let mut pr = green_pr();
    pr.checks[0].name = "Required".to_owned();
    pr.checks[0].conclusion = Some("FAILURE".to_owned());
    pr.checks[0].observed_at = Some("2026-07-25T00:00:00Z".to_owned());
    pr.checks.push(StewardCheck {
        name: "required".to_owned(),
        source: StewardCheckSource::CheckRun,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: Some(12),
        observed_at: Some("2026-07-26T00:00:00Z".to_owned()),
    });

    assert_eq!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::DirectMergeRefused {
            reasons: vec![DirectMergeRefusal::ValidatedBaseRevisionNotAtomic]
        }
    );
}

#[test]
fn genuine_failure_is_not_rerun_but_transient_is_bounded() {
    let mut pr = green_pr();
    pr.checks[0].conclusion = Some("TIMED_OUT".to_owned());
    assert_eq!(
        classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
        StewardDecision::RerunTransient { run_ids: vec![10] }
    );
    let attempts = BTreeMap::from([(10, 1)]);
    assert!(matches!(
        classify_pr(&pr, &queue_policy(), &attempts),
        StewardDecision::RequiredFailed { .. }
    ));
    pr.checks[0].conclusion = Some("FAILURE".to_owned());
    assert!(matches!(
        classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
        StewardDecision::RequiredFailed { .. }
    ));
}

#[test]
fn never_direct_merges_a_behind_private_pr() {
    let mut pr = green_pr();
    pr.merge_state = "BEHIND".to_owned();
    let mut policy = queue_policy();
    policy.merge_queue = false;
    policy.required_checks.clear();
    assert!(matches!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::NeedsUpdate { .. }
    ));
}

#[test]
fn private_exact_head_merge_never_bypasses_blocked_ruleset_state() {
    let mut pr = green_pr();
    pr.merge_state = "BLOCKED".to_owned();
    let mut policy = queue_policy();
    policy.merge_queue = false;
    assert!(matches!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::WaitingRequired { contexts }
            if contexts == vec!["github-merge-state:CLEAN (current=BLOCKED)"]
    ));
}

#[test]
fn coalesces_only_queued_superseded_pr_runs() {
    let runs = vec![
        StewardRun {
            id: 1,
            workflow_id: 8,
            run_attempt: 1,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('a'),
            head_branch: "feature".to_owned(),
            status: "in_progress".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(1),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            jobs: Vec::new(),
        },
        StewardRun {
            id: 2,
            workflow_id: 8,
            run_attempt: 1,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('a'),
            head_branch: "feature".to_owned(),
            status: "queued".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(1),
            created_at: "2026-01-01T00:01:00Z".to_owned(),
            jobs: Vec::new(),
        },
        StewardRun {
            id: 3,
            workflow_id: 9,
            run_attempt: 1,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('b'),
            head_branch: "feature".to_owned(),
            status: "queued".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(1),
            created_at: "2026-01-01T00:02:00Z".to_owned(),
            jobs: Vec::new(),
        },
    ];
    let plan = plan_run_coalescing(
        &runs,
        &BTreeMap::from([(1, sha('a'))]),
        &BTreeMap::new(),
        &BTreeSet::new(),
    );
    assert_eq!(
        plan,
        vec![RunCancellation {
            run_id: 3,
            reason: RunCancellationReason::SupersededPullRequestHead,
        }]
    );
}

#[test]
fn same_head_runs_never_authorize_cancellation() {
    let run = StewardRun {
        id: 1,
        workflow_id: 8,
        run_attempt: 1,
        workflow: "Build and Test".to_owned(),
        head_sha: sha('a'),
        head_branch: "feature".to_owned(),
        status: "queued".to_owned(),
        event: "pull_request".to_owned(),
        pull_request_number: Some(1),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        jobs: Vec::new(),
    };
    let mut later = run.clone();
    later.id = 2;
    later.created_at = "2026-01-01T00:01:00Z".to_owned();
    assert!(
        plan_run_coalescing(
            &[run, later],
            &BTreeMap::from([(1, sha('a'))]),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .is_empty()
    );
    assert!(!coalescing_reason_authorizes(
        RunCancellationReason::DuplicateImmutableHead
    ));
    assert!(coalescing_reason_authorizes(
        RunCancellationReason::SupersededPullRequestHead
    ));
    assert!(coalescing_reason_authorizes(
        RunCancellationReason::SupersededMergeGroupHead
    ));
}

#[test]
fn never_cancels_in_progress_or_non_pr_runs() {
    let runs = vec![
        StewardRun {
            id: 1,
            workflow_id: 1,
            run_attempt: 1,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('a'),
            head_branch: "feature".to_owned(),
            status: "in_progress".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(1),
            created_at: String::new(),
            jobs: Vec::new(),
        },
        StewardRun {
            id: 2,
            workflow_id: 2,
            run_attempt: 1,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('b'),
            head_branch: "main".to_owned(),
            status: "queued".to_owned(),
            event: "push".to_owned(),
            pull_request_number: None,
            created_at: String::new(),
            jobs: Vec::new(),
        },
    ];
    assert!(
        plan_run_coalescing(
            &runs,
            &BTreeMap::from([(1, sha('c'))]),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .is_empty()
    );
}

#[test]
fn coalescing_uses_pr_identity_and_honors_opt_out() {
    let runs = vec![
        StewardRun {
            id: 10,
            workflow_id: 8,
            run_attempt: 1,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('a'),
            head_branch: "same-name".to_owned(),
            status: "queued".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(1),
            created_at: String::new(),
            jobs: Vec::new(),
        },
        StewardRun {
            id: 11,
            workflow_id: 8,
            run_attempt: 1,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('b'),
            head_branch: "same-name".to_owned(),
            status: "queued".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(2),
            created_at: String::new(),
            jobs: Vec::new(),
        },
    ];
    assert!(
        plan_run_coalescing(
            &runs,
            &BTreeMap::from([(1, sha('a')), (2, sha('c'))]),
            &BTreeMap::new(),
            &BTreeSet::from([2]),
        )
        .is_empty()
    );
}

#[test]
fn head_move_a_to_b_to_a_does_not_leave_cached_superseded_proof() {
    let run = StewardRun {
        id: 12,
        workflow_id: 8,
        run_attempt: 1,
        workflow: "Build and Test".to_owned(),
        head_sha: sha('a'),
        head_branch: "feature".to_owned(),
        status: "queued".to_owned(),
        event: "pull_request".to_owned(),
        pull_request_number: Some(1),
        created_at: String::new(),
        jobs: Vec::new(),
    };
    assert!(
        plan_run_coalescing(
            &[run],
            &BTreeMap::from([(1, sha('a'))]),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .is_empty()
    );
}

fn job(name: &str, status: &str, labels: &[&str]) -> StewardJob {
    StewardJob {
        id: 1,
        name: name.to_owned(),
        status: status.to_owned(),
        conclusion: (status == "skipped").then(|| "skipped".to_owned()),
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        runner_name: None,
    }
}

fn pressure_runs() -> Vec<StewardRun> {
    vec![
        StewardRun {
            id: 100,
            workflow_id: 1,
            run_attempt: 1,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('f'),
            head_branch: "gh-readonly-queue/main/pr-7-deadbeef".to_owned(),
            status: "queued".to_owned(),
            event: "merge_group".to_owned(),
            pull_request_number: None,
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            jobs: vec![job(
                "macOS (ARM64) [local]",
                "queued",
                &["self-hosted", "pulp-build", "pulp-build-vm"],
            )],
        },
        StewardRun {
            id: 200,
            workflow_id: 2,
            run_attempt: 1,
            workflow: "Example validation".to_owned(),
            head_sha: sha('a'),
            head_branch: "feature-a".to_owned(),
            status: "in_progress".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(8),
            created_at: "2026-01-01T00:01:00Z".to_owned(),
            jobs: vec![
                job(
                    "Detect example changes",
                    "in_progress",
                    &["self-hosted", "pulp-preamble"],
                ),
                job(
                    "Validate examples (macOS)",
                    "queued",
                    &["self-hosted", "pulp-build", "pulp-build-vm"],
                ),
            ],
        },
        StewardRun {
            id: 300,
            workflow_id: 3,
            run_attempt: 1,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('b'),
            head_branch: "feature-b".to_owned(),
            status: "in_progress".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(9),
            created_at: "2026-01-01T00:02:00Z".to_owned(),
            jobs: vec![
                job("macos", "in_progress", &["self-hosted", "pulp-preamble"]),
                job(
                    "macOS (ARM64) [local]",
                    "queued",
                    &["self-hosted", "pulp-build", "pulp-build-vm"],
                ),
                job("Windows", "in_progress", &["windows-latest"]),
            ],
        },
    ]
}

#[test]
fn preempts_one_explicitly_advisory_workflow() {
    let plan = plan_capacity_preemptions(
        &pressure_runs(),
        &BTreeSet::new(),
        &CapacityPreemptionPolicy::pulp(),
        &QueueFrontPressure {
            head_sha: sha('f'),
            old_enough: true,
        },
        &BTreeSet::new(),
        usize::MAX,
    );
    assert_eq!(
        plan,
        vec![RunCancellation {
            run_id: 200,
            reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
        }]
    );
}

#[test]
fn gpu_web_plugins_is_exact_advisory_capacity_work_not_cached_supersedence() {
    let mut runs = pressure_runs();
    runs[1].workflow = "GPU Web Plugins".to_owned();
    let plan = plan_capacity_preemptions(
        &runs,
        &BTreeSet::new(),
        &CapacityPreemptionPolicy::pulp(),
        &QueueFrontPressure {
            head_sha: sha('f'),
            old_enough: true,
        },
        &BTreeSet::new(),
        1,
    );
    assert_eq!(
        plan,
        vec![RunCancellation {
            run_id: 200,
            reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
        }]
    );
}

#[test]
fn never_preempts_started_expensive_push_or_unknown_work() {
    let mut cases = Vec::new();
    let mut expensive = pressure_runs();
    expensive[1].jobs[1].status = "in_progress".to_owned();
    cases.push(expensive);
    let mut completed_linux = pressure_runs();
    completed_linux[1].jobs.push(job(
        "Linux (x64) [local]",
        "completed",
        &["self-hosted", "pulp-build-linux"],
    ));
    cases.push(completed_linux);
    let mut push = pressure_runs();
    push[1].event = "push".to_owned();
    cases.push(push);
    let mut unknown = pressure_runs();
    unknown[1].jobs.push(job(
        "mystery",
        "in_progress",
        &["self-hosted", "custom-pool"],
    ));
    cases.push(unknown);
    let mut unknown_workflow = pressure_runs();
    unknown_workflow[1].workflow = "Unclassified advisory validation".to_owned();
    cases.push(unknown_workflow);
    let mut wrong_case_workflow = pressure_runs();
    wrong_case_workflow[1].workflow = "EXAMPLE VALIDATION".to_owned();
    cases.push(wrong_case_workflow);
    let mut fake_hosted = pressure_runs();
    fake_hosted[1].jobs.push(job(
        "custom",
        "in_progress",
        &["self-hosted", "custom-latest"],
    ));
    cases.push(fake_hosted);
    let mut requested_unknown = pressure_runs();
    requested_unknown[1]
        .jobs
        .push(job("requested custom", "requested", &["custom-pool"]));
    cases.push(requested_unknown);
    let mut missing_pr_identity = pressure_runs();
    missing_pr_identity[1].pull_request_number = None;
    cases.push(missing_pr_identity);
    for runs in cases {
        let plan = plan_capacity_preemptions(
            &runs,
            &BTreeSet::new(),
            &CapacityPreemptionPolicy::pulp(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: true,
            },
            &BTreeSet::new(),
            1,
        );
        assert!(
            plan.iter().all(|cancellation| cancellation.run_id != 200),
            "unsafe run was selected: {plan:?}"
        );
    }
}

#[test]
fn requires_aged_exact_front_and_never_falls_back_to_required_work() {
    let runs = pressure_runs();
    let attempted = BTreeSet::from([preemption_key(&runs[1])]);
    let plan = plan_capacity_preemptions(
        &runs,
        &BTreeSet::new(),
        &CapacityPreemptionPolicy::pulp(),
        &QueueFrontPressure {
            head_sha: sha('f'),
            old_enough: true,
        },
        &attempted,
        1,
    );
    assert!(
        plan.is_empty(),
        "an exhausted advisory candidate must not fall back to required work"
    );
    assert!(
        plan_capacity_preemptions(
            &runs,
            &BTreeSet::new(),
            &CapacityPreemptionPolicy::pulp(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: false,
            },
            &BTreeSet::new(),
            1
        )
        .is_empty()
    );
    assert!(
        plan_capacity_preemptions(
            &runs,
            &BTreeSet::new(),
            &CapacityPreemptionPolicy::pulp(),
            &QueueFrontPressure {
                head_sha: sha('e'),
                old_enough: true,
            },
            &BTreeSet::new(),
            1
        )
        .is_empty()
    );

    let mut running_front = runs;
    running_front[0].jobs[0].status = "in_progress".to_owned();
    assert!(
        plan_capacity_preemptions(
            &running_front,
            &BTreeSet::new(),
            &CapacityPreemptionPolicy::pulp(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: true,
            },
            &BTreeSet::new(),
            1
        )
        .is_empty(),
        "an already-running queue front is not waiting for pool capacity"
    );
}

#[test]
fn queued_front_preamble_models_global_scheduler_cap_pressure() {
    let mut runs = pressure_runs();
    runs[0].jobs = vec![job(
        "resolve-provider",
        "queued",
        &["self-hosted", "pulp-preamble"],
    )];
    assert_eq!(
        plan_capacity_preemptions(
            &runs,
            &BTreeSet::new(),
            &CapacityPreemptionPolicy::pulp(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: true,
            },
            &BTreeSet::new(),
            1,
        ),
        vec![RunCancellation {
            run_id: 200,
            reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
        }]
    );
    runs[0].jobs[0].status = "requested".to_owned();
    assert_eq!(
        plan_capacity_preemptions(
            &runs,
            &BTreeSet::new(),
            &CapacityPreemptionPolicy::pulp(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: true,
            },
            &BTreeSet::new(),
            1,
        )[0]
        .run_id,
        200
    );
}

#[test]
fn completed_skipped_expensive_leg_remains_unstarted() {
    let mut runs = pressure_runs();
    runs[1].jobs[1].status = "completed".to_owned();
    runs[1].jobs[1].conclusion = Some("skipped".to_owned());
    assert_eq!(
        plan_capacity_preemptions(
            &runs,
            &BTreeSet::new(),
            &CapacityPreemptionPolicy::pulp(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: true,
            },
            &BTreeSet::new(),
            1,
        )[0]
        .run_id,
        200
    );
}

#[test]
fn opted_out_pull_request_is_never_preempted() {
    let runs = pressure_runs();
    let pressure = QueueFrontPressure {
        head_sha: sha('f'),
        old_enough: true,
    };
    let plan = plan_capacity_preemptions(
        &runs,
        &BTreeSet::from([8]),
        &CapacityPreemptionPolicy::pulp(),
        &pressure,
        &BTreeSet::new(),
        1,
    );
    assert!(
        plan.is_empty(),
        "an opted-out advisory candidate must not fall back to required work"
    );
    assert!(
        plan_capacity_preemptions(
            &runs,
            &BTreeSet::from([8, 9]),
            &CapacityPreemptionPolicy::pulp(),
            &pressure,
            &BTreeSet::new(),
            1,
        )
        .is_empty()
    );
}

#[test]
fn capacity_preemption_policy_is_explicitly_pulp_only() {
    assert_eq!(
        CapacityPreemptionPolicy::for_repository("Generous-Corp/pulp"),
        CapacityPreemptionPolicy::pulp()
    );
    assert_eq!(
        CapacityPreemptionPolicy::for_repository("Generous-Corp/forge"),
        CapacityPreemptionPolicy::disabled()
    );
    assert_eq!(
        CapacityPreemptionPolicy::for_repository("another-owner/pulp"),
        CapacityPreemptionPolicy::disabled()
    );
    assert!(
        plan_capacity_preemptions(
            &pressure_runs(),
            &BTreeSet::new(),
            &CapacityPreemptionPolicy::disabled(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: true,
            },
            &BTreeSet::new(),
            1,
        )
        .is_empty()
    );
}

fn wedge_runs() -> (StewardPullRequest, Vec<StewardRun>) {
    let mut pr = green_pr();
    pr.number = 7895;
    pr.head_sha = sha('b');
    pr.head_branch = "feature/wedge".to_owned();
    let old = StewardRun {
        id: 100,
        workflow_id: 77,
        run_attempt: 1,
        workflow: "Build and Test".to_owned(),
        head_sha: sha('a'),
        head_branch: pr.head_branch.clone(),
        status: "in_progress".to_owned(),
        event: "pull_request".to_owned(),
        pull_request_number: Some(pr.number),
        created_at: "2026-08-29T08:55:28Z".to_owned(),
        jobs: vec![StewardJob {
            id: 900,
            name: "macos".to_owned(),
            status: "in_progress".to_owned(),
            conclusion: None,
            labels: [
                "self-hosted",
                "macOS",
                "ARM64",
                "pulp-build",
                "pulp-build-vm",
                "pulp-build-pr-head",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            runner_name: Some("pulp-macos-gate-slot2".to_owned()),
        }],
    };
    let current = StewardRun {
        id: 200,
        workflow_id: old.workflow_id,
        run_attempt: 1,
        workflow: old.workflow.clone(),
        head_sha: pr.head_sha.clone(),
        head_branch: pr.head_branch.clone(),
        status: "pending".to_owned(),
        event: "pull_request".to_owned(),
        pull_request_number: Some(pr.number),
        created_at: "2026-08-29T09:00:57Z".to_owned(),
        jobs: Vec::new(),
    };
    (pr, vec![old, current])
}

fn wedge_required_checks() -> Vec<RequiredCheck> {
    vec![RequiredCheck {
        context: "macos".to_owned(),
        app_id: Some(15_368),
    }]
}

#[test]
fn stale_pr_wedge_requires_pending_zero_job_successor_and_preserves_current_head() {
    let (pr, runs) = wedge_runs();
    let plan = plan_stale_pr_run_wedges(
        "Generous-Corp/pulp",
        &runs,
        std::slice::from_ref(&pr),
        &wedge_required_checks(),
    );
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].old_run_id, 100);
    assert_eq!(plan[0].new_run_id, 200);
    assert_eq!(plan[0].new_head_sha, pr.head_sha);
    assert_eq!(plan[0].local_required_job.id, 900);

    let mut materialized = runs.clone();
    let materialized_job = materialized[0].jobs[0].clone();
    materialized[1].jobs.push(materialized_job);
    assert!(
        plan_stale_pr_run_wedges(
            "Generous-Corp/pulp",
            &materialized,
            &[pr],
            &wedge_required_checks(),
        )
        .is_empty(),
        "a successor with any materialized job is not the proven scheduler wedge"
    );
}

#[test]
fn stale_pr_wedge_rejects_head_change_push_and_merge_group() {
    let (mut pr, runs) = wedge_runs();
    pr.head_sha = sha('c');
    assert!(
        plan_stale_pr_run_wedges("Generous-Corp/pulp", &runs, &[pr], &wedge_required_checks(),)
            .is_empty()
    );

    let (pr, mut push) = wedge_runs();
    push[0].event = "push".to_owned();
    assert!(
        plan_stale_pr_run_wedges(
            "Generous-Corp/pulp",
            &push,
            std::slice::from_ref(&pr),
            &wedge_required_checks(),
        )
        .is_empty()
    );

    let (_, mut merge_group) = wedge_runs();
    merge_group[0].event = "merge_group".to_owned();
    merge_group[0].head_branch = "gh-readonly-queue/main/pr-7895-deadbeef".to_owned();
    assert!(
        plan_stale_pr_run_wedges(
            "Generous-Corp/pulp",
            &merge_group,
            &[pr],
            &wedge_required_checks(),
        )
        .is_empty(),
        "merge-group cleanup remains a separate fully-paginated queue-absence rule"
    );
}

#[test]
fn stale_pr_wedge_is_pulp_macos_policy_only() {
    let (pr, runs) = wedge_runs();
    assert!(
        plan_stale_pr_run_wedges(
            "another-owner/pulp",
            &runs,
            std::slice::from_ref(&pr),
            &wedge_required_checks(),
        )
        .is_empty()
    );
    assert!(
        plan_stale_pr_run_wedges(
            "Generous-Corp/pulp",
            &runs,
            &[pr],
            &[RequiredCheck {
                context: "linux".to_owned(),
                app_id: None,
            }],
        )
        .is_empty()
    );
}
