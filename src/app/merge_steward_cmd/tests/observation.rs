use super::*;
use crate::app::merge_steward_cmd::observation::{
    canonical_repo_name, encode_path_segment, evaluated_required_checks,
    hydrate_required_check_identities, parse_check, parse_rest_check,
};
#[cfg(unix)]
use crate::app::merge_steward_cmd::observation::{
    check_runs_for_head, hydrate_preemption_jobs, required_checks,
};

#[test]
fn parses_both_check_rollup_shapes() {
    let check = parse_check(&serde_json::json!({
        "__typename": "CheckRun",
        "name": "macos",
        "status": "COMPLETED",
        "conclusion": "SUCCESS",
        "detailsUrl": "https://github.com/o/r/actions/runs/123/job/456"
    }))
    .expect("check");
    assert_eq!(check.run_id, Some(123));
    assert_eq!(check.app_id, None);
    let context = parse_check(&serde_json::json!({
        "__typename": "StatusContext",
        "context": "freeze",
        "state": "PENDING",
        "targetUrl": "https://github.com/o/r/actions/runs/789"
    }))
    .expect("context");
    assert_eq!(context.status, "IN_PROGRESS");
    assert_eq!(context.run_id, Some(789));
}

#[test]
fn active_check_uses_started_at_when_completed_at_is_null() {
    let check = parse_check(&serde_json::json!({
        "__typename": "CheckRun",
        "name": "macos",
        "status": "IN_PROGRESS",
        "conclusion": null,
        "completedAt": null,
        "startedAt": "2026-07-26T02:00:00Z"
    }))
    .expect("active check");
    assert_eq!(check.observed_at.as_deref(), Some("2026-07-26T02:00:00Z"));
}

#[test]
fn completed_check_prefers_completed_at_over_started_at() {
    let check = parse_check(&serde_json::json!({
        "__typename": "CheckRun",
        "name": "macos",
        "status": "COMPLETED",
        "conclusion": "SUCCESS",
        "startedAt": "2026-07-26T01:00:00Z",
        "completedAt": "2026-07-26T02:00:00Z"
    }))
    .expect("completed check");
    assert_eq!(check.observed_at.as_deref(), Some("2026-07-26T02:00:00Z"));
}

#[test]
fn repository_settings_supply_canonical_guard_identity() {
    assert_eq!(
        canonical_repo_name(&serde_json::json!({"full_name": "Owner/Repo"})).expect("canonical"),
        "Owner/Repo"
    );
    assert!(canonical_repo_name(&serde_json::json!({})).is_err());
    assert!(canonical_repo_name(&serde_json::json!({"full_name": "owner/repo/extra"})).is_err());
}

#[test]
fn invalid_pr_head_skips_app_identity_hydration_without_blocking_repo() {
    let actions = GitHubActions::new(".");
    let mut prs = vec![ObservedPr {
        node_id: "PR_bad".to_owned(),
        fact: StewardPullRequest {
            number: 42,
            head_sha: "malformed".to_owned(),
            head_branch: "topic".to_owned(),
            draft: false,
            merge_state: "UNKNOWN".to_owned(),
            auto_merge_active: false,
            queue_position: None,
            labels: Vec::new(),
            checks: Vec::new(),
        },
        check_rollup_maybe_truncated: false,
    }];
    hydrate_required_check_identities(
        &actions,
        "owner/repo",
        &[RequiredCheck {
            context: "macos".to_owned(),
            app_id: Some(42),
        }],
        &mut prs,
    )
    .expect("invalid head is classified locally");
    assert!(prs[0].fact.checks.is_empty());
}

#[test]
fn entitlement_match_is_exact_enough_not_to_swallow_generic_forbidden() {
    assert!(is_private_free_entitlement(
        "Upgrade to GitHub Pro or make this repository public to enable this feature."
    ));
    assert!(!is_private_free_entitlement("HTTP 403 forbidden"));
    assert!(is_admin_protection_denied(
        "HTTP 403: Must have admin rights to Repository"
    ));
    assert!(!is_admin_protection_denied("HTTP 403 forbidden"));
}

#[test]
fn job_parser_and_reason_labels_fail_closed_and_stay_stable() {
    let parsed = parse_job(&serde_json::json!({
        "name": "macos",
        "status": "in_progress",
        "labels": ["self-hosted", "pulp-preamble"],
        "runner_name": "pulp-preamble-m5"
    }))
    .expect("job");
    assert_eq!(parsed.labels[1], "pulp-preamble");
    assert!(parse_job(&serde_json::json!({"status": "queued"})).is_err());
    assert_eq!(
        cancellation_reason_label(RunCancellationReason::LowerPriorityBranchPreamble),
        "lower_priority_branch_preamble"
    );
}

#[test]
fn evaluated_rules_extract_required_checks_and_reject_malformed_payloads() {
    let checks = evaluated_required_checks(&serde_json::json!([[
            {
                "type": "required_status_checks",
                "parameters": {
                    "required_status_checks": [
                        {"context": "macos"},
                        {"context": "macos", "integration_id": 42},
                        {"context": "linux"},
                        {"context": "any-app", "integration_id": -1},
                        {"context": "macos", "integration_id": 42}
                    ]
                }
            }
        ],
        [
            {"type": "pull_request", "parameters": {"required_approving_review_count": 1}}
        ]
    ]))
    .expect("rules");
    assert_eq!(
        checks,
        vec![
            RequiredCheck {
                context: "any-app".to_owned(),
                app_id: None,
            },
            RequiredCheck {
                context: "linux".to_owned(),
                app_id: None,
            },
            RequiredCheck {
                context: "macos".to_owned(),
                app_id: Some(42),
            },
        ]
    );
    assert!(evaluated_required_checks(&serde_json::json!({})).is_err());
    assert!(
        evaluated_required_checks(&serde_json::json!([{
            "type": "required_status_checks",
            "parameters": {"required_status_checks": [
                {"context": "macos", "integration_id": "unknown"}
            ]}
        }]))
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn required_context_transport_unions_classic_checks_and_paginated_rules() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"protection/required_status_checks"*)
    printf '%s' '{"contexts":["classic","app-bound"],"checks":[{"context":"app-bound","app_id":42}]}' ;;
  *"rules/branches/main --paginate --slurp"*)
    printf '%s' '[[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"rules-a"}]}}],[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"rules-b"}]}}]]' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );

    assert_eq!(
        required_checks(&actions, "owner/repo", "main").expect("required checks"),
        vec![
            RequiredCheck {
                context: "app-bound".to_owned(),
                app_id: Some(42),
            },
            RequiredCheck {
                context: "classic".to_owned(),
                app_id: None,
            },
            RequiredCheck {
                context: "rules-a".to_owned(),
                app_id: None,
            },
            RequiredCheck {
                context: "rules-b".to_owned(),
                app_id: None,
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn dispatch_runner_inventory_is_paginated_and_preserves_registered_state() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"actions/runners?per_page=100&page=1"*)
    printf '{"runners":['
    i=1
    while [ "$i" -le 100 ]; do
      [ "$i" -eq 1 ] || printf ','
      printf '{"id":%s,"name":"runner-%s","status":"online","busy":false,"labels":[{"name":"self-hosted"},{"name":"macOS"}]}' "$i" "$i"
      i=$((i + 1))
    done
    printf ']}' ;;
  *"actions/runners?per_page=100&page=2"*)
    printf '%s' '{"runners":[{"id":101,"name":"runner-101","status":"offline","busy":true,"labels":[{"name":"self-hosted"}]}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let runners = dispatch_runner_observations(&actions, "owner/repo").expect("runner inventory");
    assert_eq!(runners.len(), 101);
    assert_eq!(runners[0].runner_id, 1);
    assert_eq!(runners[0].status, "online");
    assert!(!runners[0].busy);
    assert_eq!(runners[100].runner_id, 101);
    assert!(runners[100].busy);
}

#[cfg(unix)]
#[test]
fn dispatch_runner_inventory_refuses_missing_envelope() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, "printf '%s' '{}'");
    assert!(dispatch_runner_observations(&actions, "owner/repo").is_err());
}

#[cfg(unix)]
#[test]
fn dispatch_job_inventory_is_bound_to_exact_run_attempt() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"actions/runs/77/attempts/3/jobs?per_page=100&page=1"*)
    printf '%s' '{"jobs":[{"id":303,"name":"macos","status":"queued","conclusion":null,"labels":["self-hosted","macos"],"runner_name":null}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let jobs = crate::app::merge_steward_cmd::observation::fetch_run_attempt_jobs(
        &actions,
        "owner/repo",
        77,
        3,
    )
    .expect("attempt jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, 303);
}

#[cfg(unix)]
#[test]
fn dispatch_check_producer_inventory_binds_run_job_and_app() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"printf '%s' '{"check_runs":[{"name":"macos","app":{"id":42},"details_url":"https://github.com/owner/repo/actions/runs/77/job/303"}]}'"#,
    );
    let producers = crate::app::merge_steward_cmd::observation::job_check_producers_for_head(
        &actions,
        "owner/repo",
        &"a".repeat(40),
    )
    .expect("producer inventory");
    let producer = producers.get(&303).expect("job producer");
    assert_eq!(producer.run_id, 77);
    assert_eq!(producer.job_id, 303);
    assert_eq!(producer.name, "macos");
    assert_eq!(producer.app_id, Some(42));
}

#[cfg(unix)]
#[test]
fn dispatch_job_detail_vetoes_assignment_or_completion_race() {
    let listed = StewardJob {
        id: 303,
        name: "macos".to_owned(),
        status: "queued".to_owned(),
        conclusion: None,
        labels: vec!["self-hosted".to_owned(), "macos".to_owned()],
        runner_name: None,
    };
    let required = vec![RequiredCheck {
        context: "macos".to_owned(),
        app_id: Some(42),
    }];
    let producers = BTreeMap::from([(
        303,
        crate::app::merge_steward_cmd::observation::JobCheckProducer {
            run_id: 77,
            job_id: 303,
            name: "macos".to_owned(),
            app_id: Some(42),
        },
    )]);
    assert!(current_required_dispatch_job(&listed, &listed, 77, &required, &producers).is_some());

    let mut assigned = listed.clone();
    assigned.status = "in_progress".to_owned();
    assigned.runner_name = Some("m3-pulp-gate-01".to_owned());
    assert!(current_required_dispatch_job(&listed, &assigned, 77, &required, &producers).is_none());

    let mut completed = listed.clone();
    completed.status = "completed".to_owned();
    completed.conclusion = Some("success".to_owned());
    assert!(
        current_required_dispatch_job(&listed, &completed, 77, &required, &producers).is_none()
    );

    let wrong_app = BTreeMap::from([(
        303,
        crate::app::merge_steward_cmd::observation::JobCheckProducer {
            run_id: 77,
            job_id: 303,
            name: "macos".to_owned(),
            app_id: Some(7),
        },
    )]);
    assert!(current_required_dispatch_job(&listed, &listed, 77, &required, &wrong_app).is_none());
}

#[cfg(unix)]
#[test]
fn one_hundred_same_repository_targets_load_shared_observation_once() {
    use std::cell::Cell;

    let targets = (1..=100)
        .map(|pull_request| DispatchWedgeTargetRequest {
            base_ref: "main".to_owned(),
            pull_request,
            expected_head_sha: format!("{pull_request:040x}"),
        })
        .collect::<Vec<_>>();
    let runner_loads = Cell::new(0);
    let repository_loads = Cell::new(0);
    let target_observations = Cell::new(0);
    let results = observe_dispatch_wedge_targets_with(
        &targets,
        || {
            runner_loads.set(runner_loads.get() + 1);
            Ok(Vec::new())
        },
        |base_ref| {
            repository_loads.set(repository_loads.get() + 1);
            Ok(base_ref.to_owned())
        },
        |shared, target, _| {
            target_observations.set(target_observations.get() + 1);
            assert_eq!(shared, &target.base_ref);
            Ok(Vec::new())
        },
    );

    assert_eq!(results.len(), 100);
    assert!(results.iter().all(|result| result.result.is_ok()));
    assert_eq!(runner_loads.get(), 1);
    assert_eq!(repository_loads.get(), 1);
    assert_eq!(target_observations.get(), 100);
}

#[cfg(unix)]
#[test]
fn one_target_observation_failure_does_not_poison_repository_batch() {
    let targets = [
        DispatchWedgeTargetRequest {
            base_ref: "main".to_owned(),
            pull_request: 42,
            expected_head_sha: "a".repeat(40),
        },
        DispatchWedgeTargetRequest {
            base_ref: "main".to_owned(),
            pull_request: 43,
            expected_head_sha: "b".repeat(40),
        },
    ];
    let results = observe_dispatch_wedge_targets_with(
        &targets,
        || Ok(Vec::new()),
        |_| Ok(()),
        |(), target, _| {
            if target.pull_request == 42 {
                Err("exact target detail failed".to_owned())
            } else {
                Ok(Vec::new())
            }
        },
    );

    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].result.as_ref().expect_err("target 42 fails"),
        "exact target detail failed"
    );
    assert!(results[1].result.is_ok());
}

#[cfg(unix)]
#[test]
fn required_context_transport_falls_back_from_admin_denial_to_evaluated_rules() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"rules/branches/main --paginate --slurp"*)
    printf '%s' '[[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"macos","integration_id":42}]}}]]' ;;
  *"protection/required_status_checks"*)
    echo "HTTP 403: Must have admin rights to Repository" >&2; exit 1 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );

    assert_eq!(
        required_checks(&actions, "owner/repo", "main").expect("evaluated fallback"),
        vec![RequiredCheck {
            context: "macos".to_owned(),
            app_id: Some(42),
        }]
    );
}

#[cfg(unix)]
#[test]
fn required_context_transport_fails_closed_when_evaluated_rules_are_unreadable() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
echo "HTTP 403: Must have admin rights to Repository" >&2
exit 1
"#,
    );

    assert!(required_checks(&actions, "owner/repo", "main").is_err());
}

#[test]
fn rest_check_parser_preserves_app_identity_and_unavailable_identity() {
    let check = parse_rest_check(&serde_json::json!({
        "name": "macos",
        "app": {"id": 42},
        "status": "completed",
        "conclusion": "success",
        "details_url": "https://github.com/o/r/actions/runs/123/job/456",
        "completed_at": "2026-07-26T02:00:00Z"
    }))
    .expect("check");
    assert_eq!(check.app_id, Some(42));
    assert_eq!(check.run_id, Some(123));

    let unavailable = parse_rest_check(&serde_json::json!({
        "name": "macos",
        "app": null,
        "status": "completed",
        "conclusion": "success"
    }))
    .expect("check without app identity");
    assert_eq!(unavailable.app_id, None);
}

#[cfg(unix)]
#[test]
fn current_head_check_transport_preserves_producer_identity() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"printf '%s' '{"check_runs":[{"name":"macos","app":{"id":42},"status":"completed","conclusion":"success","details_url":"https://github.com/o/r/actions/runs/123","completed_at":"2026-07-26T02:00:00Z"}]}'"#,
    );
    let checks = check_runs_for_head(&actions, "owner/repo", &"a".repeat(40))
        .expect("current-head check identities");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].app_id, Some(42));
}

#[cfg(unix)]
#[test]
fn live_pr_revalidation_hydrates_required_check_identity() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"pr view"*)
    printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS"}]}' ;;
  *"commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/check-runs"*)
    printf '%s' '{"check_runs":[{"name":"macos","app":{"id":42},"status":"completed","conclusion":"success"}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let required = vec![RequiredCheck {
        context: "macos".to_owned(),
        app_id: Some(42),
    }];
    let pr = pull_request_with_required_checks(
        &actions,
        "owner/repo",
        42,
        "main",
        &BTreeMap::new(),
        &required,
    )
    .expect("live PR")
    .expect("open PR");
    assert!(
        pr.fact
            .checks
            .iter()
            .any(|check| check.name == "macos" && check.app_id == Some(42))
    );
}

#[cfg(unix)]
#[test]
fn truncated_rollup_fetches_complete_head_checks_before_merge_classification() {
    let temp = tempfile::tempdir().expect("temp");
    let rollup = (0..100)
        .map(|index| {
            serde_json::json!({
                "__typename": "CheckRun",
                "name": format!("check-{index}"),
                "status": "COMPLETED",
                "conclusion": "SUCCESS"
            })
        })
        .collect::<Vec<_>>();
    let pr = serde_json::json!({
        "id": "PR_kw",
        "number": 42,
        "state": "OPEN",
        "isDraft": false,
        "baseRefName": "main",
        "headRefOid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "headRefName": "feature",
        "mergeStateStatus": "CLEAN",
        "autoMergeRequest": null,
        "labels": [],
        "statusCheckRollup": rollup
    });
    let first_page = serde_json::json!({
        "check_runs": (0..100)
            .map(|index| serde_json::json!({
                "name": format!("check-{index}"),
                "status": "completed",
                "conclusion": "success"
            }))
            .collect::<Vec<_>>()
    });
    let second_page = serde_json::json!({
        "check_runs": [{
            "name": "omitted-failure",
            "status": "completed",
            "conclusion": "failure"
        }]
    });
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
case "$*" in
  *"pr view"*) printf '%s' '{pr}' ;;
  *"/check-runs"*"&page=1"*) printf '%s' '{first_page}' ;;
  *"/check-runs"*"&page=2"*) printf '%s' '{second_page}' ;;
  *"/statuses"*) printf '%s' '[]' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#
        ),
    );
    let required = vec![RequiredCheck {
        context: "omitted-failure".to_owned(),
        app_id: None,
    }];
    let observed = pull_request_with_required_checks(
        &actions,
        "owner/repo",
        42,
        "main",
        &BTreeMap::new(),
        &required,
    )
    .expect("live PR")
    .expect("open PR");
    let mut policy = queue_policy();
    policy.merge_queue = false;
    policy.required_checks = required;
    assert_eq!(
        classify_pr(&observed.fact, &policy, &BTreeMap::new()),
        StewardDecision::RequiredFailed {
            contexts: vec!["omitted-failure".to_owned()]
        }
    );
}

#[test]
fn branch_policy_path_segments_are_percent_encoded() {
    assert_eq!(encode_path_segment("main"), "main");
    assert_eq!(encode_path_segment("release/1.2"), "release%2F1.2");
    assert_eq!(encode_path_segment("topic name"), "topic%20name");
}

#[cfg(unix)]
#[test]
fn pull_request_transport_preserves_fresh_queue_position() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[]}'"#,
    );
    let positions = BTreeMap::from([(42, 3)]);

    let pr = pull_request(&actions, "owner/repo", 42, "main", &positions)
        .expect("transport")
        .expect("open PR");
    assert_eq!(pr.fact.queue_position, Some(3));
}

#[cfg(unix)]
#[test]
fn merge_queue_transport_refuses_partial_snapshot() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":true}}}}}}'"#,
    );

    let error = merge_queue_snapshot(&actions, "owner/repo", "main").expect_err("partial");
    assert!(error.contains("exceeds 100 entries"), "{error}");
}

#[cfg(unix)]
#[test]
fn active_run_transport_deduplicates_status_and_page_overlap() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"actions/runs?status=queued"*|*"actions/runs?status=waiting"*)
    printf '%s' '{"workflow_runs":[{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":42}]}]}' ;;
  *"actions/runs?status="*) printf '%s' '{"workflow_runs":[]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );

    let runs = active_runs(&actions, "owner/repo").expect("active runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, 1);
}

#[cfg(unix)]
#[test]
fn disabled_preemption_policy_performs_no_job_hydration_reads() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, r#"echo "unexpected GitHub read" >&2; exit 2"#);
    let mut runs = vec![StewardRun {
        id: 1,
        workflow_id: 77,
        run_attempt: 1,
        workflow: "Build and Test".to_owned(),
        head_sha: "a".repeat(40),
        head_branch: "gh-readonly-queue/main/pr-42".to_owned(),
        status: "in_progress".to_owned(),
        event: "merge_group".to_owned(),
        pull_request_number: Some(42),
        created_at: "2026-07-26T00:00:00Z".to_owned(),
        jobs: Vec::new(),
    }];
    hydrate_preemption_jobs(
        &actions,
        "Generous-Corp/forge",
        Some(&"a".repeat(40)),
        &CapacityPreemptionPolicy::disabled(),
        &mut runs,
    )
    .expect("disabled policy skips hydration");
    assert!(runs[0].jobs.is_empty());
}
