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
        max_transient_reruns: 1,
    }
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
fn ignores_advisory_failure_when_required_context_is_green() {
    let mut pr = green_pr();
    pr.checks.push(StewardCheck {
        name: "advisory".to_owned(),
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
fn private_free_repo_requires_all_observed_checks_and_exact_head_merge() {
    let mut policy = queue_policy();
    policy.merge_queue = false;
    policy.native_auto_merge = false;
    policy.required_checks.clear();
    assert_eq!(
        classify_pr(&green_pr(), &policy, &BTreeMap::new()),
        StewardDecision::ExactHeadMerge
    );
    let mut red = green_pr();
    red.checks[0].conclusion = Some("FAILURE".to_owned());
    assert!(matches!(
        classify_pr(&red, &policy, &BTreeMap::new()),
        StewardDecision::RequiredFailed { .. }
    ));
}

#[test]
fn private_free_repo_uses_newest_duplicate_observed_check() {
    let mut policy = queue_policy();
    policy.merge_queue = false;
    policy.native_auto_merge = false;
    policy.required_checks.clear();
    let mut pr = green_pr();
    pr.checks[0].name = "Required".to_owned();
    pr.checks[0].conclusion = Some("FAILURE".to_owned());
    pr.checks[0].observed_at = Some("2026-07-25T00:00:00Z".to_owned());
    pr.checks.push(StewardCheck {
        name: "required".to_owned(),
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: Some(12),
        observed_at: Some("2026-07-26T00:00:00Z".to_owned()),
    });

    assert_eq!(
        classify_pr(&pr, &policy, &BTreeMap::new()),
        StewardDecision::ExactHeadMerge
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
fn coalesces_only_queued_duplicate_and_superseded_pr_runs() {
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
        vec![
            RunCancellation {
                run_id: 2,
                reason: RunCancellationReason::DuplicateImmutableHead,
            },
            RunCancellation {
                run_id: 3,
                reason: RunCancellationReason::SupersededPullRequestHead,
            },
        ]
    );
}

#[test]
fn repeated_observation_of_same_run_id_is_not_a_duplicate_run() {
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
    assert!(
        plan_run_coalescing(
            &[run.clone(), run],
            &BTreeMap::from([(1, sha('a'))]),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .is_empty()
    );
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

fn current_heads() -> BTreeMap<u64, String> {
    BTreeMap::from([(8, sha('a')), (9, sha('c'))])
}

#[test]
fn preempts_one_advisory_before_lower_priority_branch() {
    let plan = plan_capacity_preemptions(
        &pressure_runs(),
        &current_heads(),
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
        &current_heads(),
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
            &current_heads(),
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
fn requires_aged_exact_front_and_durable_attempt_budget() {
    let runs = pressure_runs();
    let attempted = BTreeSet::from([preemption_key(&runs[1])]);
    let plan = plan_capacity_preemptions(
        &runs,
        &current_heads(),
        &BTreeSet::new(),
        &CapacityPreemptionPolicy::pulp(),
        &QueueFrontPressure {
            head_sha: sha('f'),
            old_enough: true,
        },
        &attempted,
        1,
    );
    assert_eq!(plan[0].run_id, 300);
    let wrong_pr_identity = BTreeMap::from([(10, sha('c'))]);
    assert!(
        plan_capacity_preemptions(
            &runs,
            &wrong_pr_identity,
            &BTreeSet::new(),
            &CapacityPreemptionPolicy::pulp(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: true,
            },
            &attempted,
            1,
        )
        .is_empty(),
        "a same-name branch from another PR must not prove stale identity"
    );
    assert!(
        plan_capacity_preemptions(
            &runs,
            &current_heads(),
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
            &current_heads(),
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
            &current_heads(),
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
            &current_heads(),
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
            &current_heads(),
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
            &current_heads(),
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
        &current_heads(),
        &BTreeSet::from([8]),
        &CapacityPreemptionPolicy::pulp(),
        &pressure,
        &BTreeSet::new(),
        1,
    );
    assert_eq!(plan[0].run_id, 300);
    assert!(
        plan_capacity_preemptions(
            &runs,
            &current_heads(),
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
            &current_heads(),
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
