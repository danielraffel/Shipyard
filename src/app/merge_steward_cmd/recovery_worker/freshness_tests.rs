use super::*;

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

#[test]
fn newest_status_wins_even_when_api_order_is_hostile() {
    let statuses = vec![
        serde_json::json!({
            "id": 10,
            "context": super::super::HANDOFF_CONTEXT,
            "state": "failure",
            "created_at": "2026-08-21T01:00:00Z"
        }),
        serde_json::json!({
            "id": 11,
            "context": super::super::HANDOFF_CONTEXT,
            "state": "success",
            "created_at": "2026-08-21T02:00:00Z"
        }),
    ];
    assert_eq!(
        github::latest_status_state(&statuses, super::super::HANDOFF_CONTEXT)
            .expect("latest status"),
        Some("success")
    );
    let reversed = statuses.into_iter().rev().collect::<Vec<_>>();
    assert_eq!(
        github::latest_status_state(&reversed, super::super::HANDOFF_CONTEXT)
            .expect("latest reversed status"),
        Some("success")
    );
    let malformed = vec![serde_json::json!({
        "id": 12,
        "context": super::super::HANDOFF_CONTEXT,
        "state": "success",
        "created_at": "not-a-timestamp"
    })];
    assert!(github::latest_status_state(&malformed, super::super::HANDOFF_CONTEXT).is_err());

    let missing_context = vec![serde_json::json!({
        "id": 13,
        "state": "success",
        "created_at": "2026-08-21T02:00:00Z"
    })];
    assert!(github::latest_status_state(&missing_context, super::super::HANDOFF_CONTEXT).is_err());

    let unknown_state = vec![serde_json::json!({
        "id": 14,
        "context": super::super::RECOVERY_CONTEXT,
        "state": "mystery",
        "created_at": "2026-08-21T02:00:00Z"
    })];
    assert!(github::latest_status_state(&unknown_state, super::super::RECOVERY_CONTEXT).is_err());

    let repeated_id = vec![
        serde_json::json!({
            "id": 15,
            "context": super::super::RECOVERY_CONTEXT,
            "state": "failure",
            "created_at": "2026-08-21T02:00:00Z"
        }),
        serde_json::json!({
            "id": 15,
            "context": super::super::RECOVERY_CONTEXT,
            "state": "success",
            "created_at": "2026-08-21T02:00:00Z"
        }),
    ];
    assert!(github::latest_status_state(&repeated_id, super::super::RECOVERY_CONTEXT).is_err());

    let same_second = vec![
        serde_json::json!({
            "id": 20,
            "context": super::super::HANDOFF_CONTEXT,
            "state": "failure",
            "created_at": "2026-08-21T03:00:00Z"
        }),
        serde_json::json!({
            "id": 21,
            "context": super::super::HANDOFF_CONTEXT,
            "state": "success",
            "created_at": "2026-08-21T03:00:00Z"
        }),
    ];
    for statuses in [same_second.clone(), same_second.into_iter().rev().collect()] {
        assert_eq!(
            github::latest_status_state(&statuses, super::super::HANDOFF_CONTEXT)
                .expect("same-second status tie"),
            Some("success")
        );
    }
}

#[test]
fn status_pagination_is_explicitly_bounded_and_finds_later_contexts() {
    let full_irrelevant_page = |page: u32| {
        (0..github::STATUS_PAGE_SIZE)
            .map(|offset| {
                serde_json::json!({
                    "context": format!("ci/{page}/{offset}")
                })
            })
            .collect::<Vec<_>>()
    };
    let mut collected = Vec::new();
    assert!(
        !github::append_status_page(&mut collected, &full_irrelevant_page(1), 1)
            .expect("continue after full first page")
    );
    let target_page = vec![
        serde_json::json!({"context": super::super::HANDOFF_CONTEXT}),
        serde_json::json!({"context": super::super::RECOVERY_CONTEXT}),
    ];
    assert!(
        github::append_status_page(&mut collected, &target_page, 2)
            .expect("stop after both target contexts")
    );

    let mut truncated = Vec::new();
    for page in 1..github::MAX_STATUS_PAGES {
        assert!(
            !github::append_status_page(&mut truncated, &full_irrelevant_page(page), page)
                .expect("bounded page")
        );
    }
    let error = github::append_status_page(
        &mut truncated,
        &full_irrelevant_page(github::MAX_STATUS_PAGES),
        github::MAX_STATUS_PAGES,
    )
    .expect_err("full bounded window must fail closed");
    assert!(error.message().contains("bounded 400-status window"));
}

#[test]
fn malformed_pull_freshness_cannot_supersede_a_request() {
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    let labels = serde_json::json!([
        {"name": super::super::MANAGED_LABEL},
        {"name": super::super::NEEDS_AGENT_LABEL}
    ]);
    let malformed = [
        serde_json::json!({
            "headRefOid": request.head_sha,
            "labels": labels
        }),
        serde_json::json!({
            "state": "OPEN",
            "labels": labels
        }),
        serde_json::json!({
            "state": "OPEN",
            "baseRefName": "main",
            "headRefOid": "partial",
            "labels": labels
        }),
        serde_json::json!({
            "state": "MYSTERY",
            "baseRefName": "main",
            "headRefOid": request.head_sha,
            "labels": labels
        }),
        serde_json::json!({
            "state": "OPEN",
            "baseRefName": "main",
            "headRefOid": request.head_sha
        }),
        serde_json::json!({
            "state": "OPEN",
            "baseRefName": "main",
            "headRefOid": request.head_sha,
            "labels": [{}]
        }),
    ];
    for response in malformed {
        assert!(
            github::classify_pull_response(&response, &request, &[]).is_err(),
            "malformed response must fail without a superseding disposition: {response}"
        );
    }

    let closed = serde_json::json!({"state": "CLOSED"});
    assert!(matches!(
        github::classify_pull_response(&closed, &request, &[]).expect("valid closed response"),
        RequestDisposition::Superseded(_)
    ));
}

#[test]
fn pull_freshness_binds_base_and_rechecks_recorded_failure() {
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    let response = |base: &str, status: &str, conclusion: Option<&str>| {
        serde_json::json!({
            "state": "OPEN",
            "isDraft": false,
            "baseRefName": base,
            "headRefOid": request.head_sha,
            "mergeStateStatus": "CLEAN",
            "labels": [
                {"name": super::super::MANAGED_LABEL},
                {"name": super::super::NEEDS_AGENT_LABEL}
            ],
            "statusCheckRollup": [{
                "__typename": "CheckRun",
                "name": "macos",
                "status": status,
                "conclusion": conclusion,
                "completedAt": "2026-08-21T08:00:00Z"
            }]
        })
    };

    assert!(matches!(
        github::classify_pull_response(
            &response("main", "COMPLETED", Some("FAILURE")),
            &request,
            &[],
        )
        .expect("recorded failure remains current"),
        RequestDisposition::Current
    ));
    for stale in [
        response("main", "COMPLETED", Some("SUCCESS")),
        response("main", "IN_PROGRESS", None),
        response("release", "COMPLETED", Some("FAILURE")),
    ] {
        assert!(matches!(
            github::classify_pull_response(&stale, &request, &[]).expect("stale evidence is typed"),
            RequestDisposition::Superseded(_)
        ));
    }

    let literal_request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "literal-fingerprint",
        "required check failed",
        required_policy("lint (app_id=7)"),
        vec![required_check("lint (app_id=7)")],
        "steward-policy",
        "worker-config",
    )
    .expect("literal check request");
    let mut literal_response = response("main", "COMPLETED", Some("FAILURE"));
    literal_response["statusCheckRollup"][0]["name"] = Value::String("lint (app_id=7)".to_owned());
    assert!(matches!(
        github::classify_pull_response(&literal_response, &literal_request, &[])
            .expect("literal app-like suffix remains literal"),
        RequestDisposition::Current
    ));
}

#[test]
fn merge_state_freshness_matches_steward_queue_precedence() {
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    let response = |merge_state: &str| {
        serde_json::json!({
            "state": "OPEN",
            "isDraft": false,
            "baseRefName": "main",
            "headRefOid": request.head_sha,
            "mergeStateStatus": merge_state,
            "labels": [
                {"name": super::super::MANAGED_LABEL},
                {"name": super::super::NEEDS_AGENT_LABEL}
            ],
            "statusCheckRollup": [{
                "__typename": "CheckRun",
                "name": "macos",
                "status": "COMPLETED",
                "conclusion": "FAILURE",
                "completedAt": "2026-08-21T08:00:00Z"
            }]
        })
    };
    let merge_request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "merge-fingerprint",
        "merge conflict",
        Vec::new(),
        vec![RecoveryFailureFact::MergeState {
            state: "DIRTY".to_owned(),
        }],
        "steward-policy",
        "worker-config",
    )
    .expect("merge request");
    assert!(matches!(
        github::classify_pull_response(&response("DIRTY"), &merge_request, &[])
            .expect("merge-state failure remains current"),
        RequestDisposition::Current
    ));
    assert!(matches!(
        github::classify_pull_response(&response("CLEAN"), &merge_request, &[])
            .expect("recovered merge state is typed"),
        RequestDisposition::Superseded(_)
    ));

    let queue_request = RecoveryRequest::new_with_steward_policy(
        &request.repo,
        request.pr,
        &request.base_ref,
        &request.head_sha,
        true,
        "shipyard:no-auto-merge",
        &request.failure_fingerprint,
        &request.failure_summary,
        request.required_checks.clone(),
        request.failure_facts.clone(),
        &request.policy_signature,
        &request.config_signature,
    )
    .expect("merge-queue request");
    let behind = response("BEHIND");
    assert!(matches!(
        github::classify_pull_response(&behind, &queue_request, &[])
            .expect("merge-queue precedence remains deterministic"),
        RequestDisposition::Current
    ));
    assert!(matches!(
        github::classify_pull_response(&behind, &request, &[])
            .expect("non-queue behind state takes precedence"),
        RequestDisposition::Superseded(_)
    ));
}

#[test]
fn changed_transient_required_failures_remain_deterministic_steward_work() {
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    for conclusion in ["CANCELLED", "TIMED_OUT", "STARTUP_FAILURE", "STALE"] {
        let response = serde_json::json!({
            "state": "OPEN",
            "isDraft": false,
            "baseRefName": "main",
            "headRefOid": request.head_sha,
            "mergeStateStatus": "CLEAN",
            "labels": [
                {"name": super::super::MANAGED_LABEL},
                {"name": super::super::NEEDS_AGENT_LABEL}
            ],
            "statusCheckRollup": [{
                "__typename": "CheckRun",
                "name": "macos",
                "status": "COMPLETED",
                "conclusion": conclusion,
                "detailsUrl": "https://github.com/owner/repo/actions/runs/123",
                "completedAt": "2026-08-21T09:00:00Z"
            }]
        });
        assert!(matches!(
            github::classify_pull_response(&response, &request, &[])
                .expect("transient retry semantics remain steward-owned"),
            RequestDisposition::Superseded(_)
        ));
    }
}

#[test]
fn exact_transient_failure_identity_remains_current_after_steward_budget_exhaustion() {
    for conclusion in ["CANCELLED", "TIMED_OUT", "STARTUP_FAILURE", "STALE"] {
        let request = RecoveryRequest::new(
            "Generous-Corp/pulp",
            42,
            "main",
            "0123456789abcdef0123456789abcdef01234567",
            "failure-fingerprint",
            "required check failed",
            required_policy("macos"),
            vec![RecoveryFailureFact::RequiredCheck {
                context: "macos".to_owned(),
                app_id: None,
                conclusion: conclusion.to_owned(),
                run_id: Some(123),
            }],
            "steward-policy",
            "worker-config",
        )
        .expect("request");
        let response = |run_id| {
            serde_json::json!({
                "state": "OPEN",
                "isDraft": false,
                "baseRefName": "main",
                "headRefOid": request.head_sha,
                "mergeStateStatus": "CLEAN",
                "labels": [
                    {"name": super::super::MANAGED_LABEL},
                    {"name": super::super::NEEDS_AGENT_LABEL}
                ],
                "statusCheckRollup": [{
                    "__typename": "CheckRun",
                    "name": "macos",
                    "status": "COMPLETED",
                    "conclusion": conclusion,
                    "detailsUrl": format!("https://github.com/owner/repo/actions/runs/{run_id}"),
                    "completedAt": "2026-08-21T09:00:00Z"
                }]
            })
        };
        assert!(matches!(
            github::classify_pull_response(&response(123), &request, &[])
                .expect("same exhausted transient run remains current"),
            RequestDisposition::Current
        ));
        assert!(matches!(
            github::classify_pull_response(&response(124), &request, &[])
                .expect("a different transient run changes deterministic evidence"),
            RequestDisposition::Superseded(_)
        ));
    }

    let no_run_request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "no-run-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![RecoveryFailureFact::RequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
            conclusion: "STARTUP_FAILURE".to_owned(),
            run_id: None,
        }],
        "steward-policy",
        "worker-config",
    )
    .expect("no-run request");
    let no_run_response = serde_json::json!({
        "state": "OPEN",
        "isDraft": false,
        "baseRefName": "main",
        "headRefOid": no_run_request.head_sha,
        "mergeStateStatus": "CLEAN",
        "labels": [
            {"name": super::super::MANAGED_LABEL},
            {"name": super::super::NEEDS_AGENT_LABEL}
        ],
        "statusCheckRollup": [{
            "__typename": "CheckRun",
            "name": "macos",
            "status": "COMPLETED",
            "conclusion": "STARTUP_FAILURE",
            "completedAt": "2026-08-21T09:00:00Z"
        }]
    });
    assert!(matches!(
        github::classify_pull_response(&no_run_response, &no_run_request, &[])
            .expect("same no-run transient failure remains current"),
        RequestDisposition::Current
    ));
}

#[test]
fn pull_freshness_requires_the_complete_failed_required_check_set() {
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        vec![
            RecoveryRequiredCheck {
                context: "macos".to_owned(),
                app_id: None,
            },
            RecoveryRequiredCheck {
                context: "signing".to_owned(),
                app_id: None,
            },
        ],
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    let response = |signing_conclusion: &str, advisory_conclusion: &str| {
        serde_json::json!({
            "state": "OPEN",
            "isDraft": false,
            "baseRefName": "main",
            "headRefOid": request.head_sha,
            "mergeStateStatus": "CLEAN",
            "labels": [
                {"name": super::super::MANAGED_LABEL},
                {"name": super::super::NEEDS_AGENT_LABEL}
            ],
            "statusCheckRollup": [
                {
                    "__typename": "CheckRun",
                    "name": "macos",
                    "status": "COMPLETED",
                    "conclusion": "FAILURE",
                    "completedAt": "2026-08-21T08:00:00Z"
                },
                {
                    "__typename": "CheckRun",
                    "name": "signing",
                    "status": "COMPLETED",
                    "conclusion": signing_conclusion,
                    "completedAt": "2026-08-21T08:00:00Z"
                },
                {
                    "__typename": "CheckRun",
                    "name": "advisory",
                    "status": "COMPLETED",
                    "conclusion": advisory_conclusion,
                    "completedAt": "2026-08-21T08:00:00Z"
                }
            ]
        })
    };

    assert!(matches!(
        github::classify_pull_response(&response("SUCCESS", "FAILURE"), &request, &[])
            .expect("advisory failure does not alter required evidence"),
        RequestDisposition::Current
    ));
    assert!(matches!(
        github::classify_pull_response(&response("FAILURE", "SUCCESS"), &request, &[])
            .expect("new required failure supersedes the request"),
        RequestDisposition::Superseded(_)
    ));

    let mut pending_required = response("SUCCESS", "SUCCESS");
    pending_required["statusCheckRollup"][1]["status"] = Value::String("IN_PROGRESS".to_owned());
    pending_required["statusCheckRollup"][1]["conclusion"] = Value::Null;
    assert!(matches!(
        github::classify_pull_response(&pending_required, &request, &[])
            .expect("pending required check changes the steward decision"),
        RequestDisposition::Superseded(_)
    ));

    let mut missing_required = response("SUCCESS", "SUCCESS");
    missing_required["statusCheckRollup"]
        .as_array_mut()
        .expect("check rollup")
        .remove(1);
    assert!(matches!(
        github::classify_pull_response(&missing_required, &request, &[])
            .expect("missing required check changes the steward decision"),
        RequestDisposition::Superseded(_)
    ));

    let mut merge_failure = response("SUCCESS", "SUCCESS");
    merge_failure["mergeStateStatus"] = Value::String("DIRTY".to_owned());
    assert!(matches!(
        github::classify_pull_response(&merge_failure, &request, &[])
            .expect("merge failure takes precedence over required-check recovery"),
        RequestDisposition::Superseded(_)
    ));
}

#[test]
fn pull_freshness_preserves_case_insensitive_label_identity() {
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        required_policy("macos"),
        vec![required_check("macos")],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    let response = serde_json::json!({
        "state": "OPEN",
        "isDraft": false,
        "baseRefName": "main",
        "headRefOid": request.head_sha,
        "mergeStateStatus": "CLEAN",
        "labels": [
            {"name": "Shipyard:Managed"},
            {"name": "Shipyard:Needs-Agent"}
        ],
        "statusCheckRollup": [{
            "__typename": "CheckRun",
            "name": "macos",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
            "completedAt": "2026-08-21T08:00:00Z"
        }]
    });
    assert!(matches!(
        github::classify_pull_response(&response, &request, &[])
            .expect("GitHub label identity is case-insensitive"),
        RequestDisposition::Current
    ));
}
