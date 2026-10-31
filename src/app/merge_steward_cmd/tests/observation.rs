use super::*;
use crate::app::merge_steward_cmd::observation::{
    canonical_repo_name, encode_path_segment, evaluated_required_checks,
    hydrate_required_check_identities, parse_check, parse_rest_check,
};
#[cfg(unix)]
use crate::app::merge_steward_cmd::observation::{
    check_runs_for_head, hydrate_census_check_identities, hydrate_preemption_jobs, required_checks,
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
fn run_parser_requires_explicit_positive_attempt_identity() {
    let base = serde_json::json!({
        "id": 1,
        "workflow_id": 2,
        "name": "Required",
        "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "head_branch": "feature",
        "status": "queued",
        "event": "pull_request",
        "created_at": "2026-08-31T19:00:00Z",
        "pull_requests": []
    });
    assert!(crate::app::merge_steward_cmd::observation::parse_run(&base).is_none());
    let mut zero = base.clone();
    zero.as_object_mut()
        .unwrap()
        .insert("run_attempt".to_owned(), serde_json::json!(0));
    assert!(crate::app::merge_steward_cmd::observation::parse_run(&zero).is_none());
    let mut malformed = base;
    malformed
        .as_object_mut()
        .unwrap()
        .insert("run_attempt".to_owned(), serde_json::json!("1"));
    assert!(crate::app::merge_steward_cmd::observation::parse_run(&malformed).is_none());
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
    let actions = GitHubActions::offline_for_tests(".");
    let mut prs = vec![ObservedPr {
        node_id: "PR_bad".to_owned(),
        owner: Some("team-lead".to_owned()),
        fact: StewardPullRequest {
            number: 42,
            head_sha: "malformed".to_owned(),
            author: None,
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

/// The REST list endpoint carries no `mergeable_state`; only `pulls/{n}` does.
/// Rows below follow that real shape.
#[cfg(unix)]
const REST_LIST_ROWS: &str = r#"[
{"node_id":"PR_ready","number":42,"draft":false,"base":{"ref":"main"},"head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ref":"feature"},"auto_merge":null,"labels":[{"name":"managed"}]},
{"node_id":"PR_draft","number":43,"draft":true,"base":{"ref":"main"},"head":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","ref":"draft"},"auto_merge":null,"labels":[]},
{"node_id":"PR_queued","number":44,"draft":false,"base":{"ref":"main"},"head":{"sha":"cccccccccccccccccccccccccccccccccccccccc","ref":"queued"},"auto_merge":null,"labels":[]}
]"#;

#[cfg(unix)]
#[test]
fn open_pr_list_falls_back_to_rest_after_gateway_failure() {
    let temp = tempfile::tempdir().expect("temp");
    let calls_path = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"printf '%s\n' "$*" >> '{calls}'
case "$*" in
  *"pr list"*) echo "HTTP 502 Bad Gateway" >&2; exit 1 ;;
  *"repos/owner/repo/pulls?state=open&base=main&per_page=100&page=1"*)
    printf '%s' '{REST_LIST_ROWS}' ;;
  *"repos/owner/repo/pulls/42") printf '%s' '{{"number":42,"mergeable_state":"clean","head":{{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            calls = calls_path.display(),
        ),
    );
    let positions = BTreeMap::from([(44, 0)]);
    let prs = pull_requests(&actions, "owner/repo", "main", &positions).expect("REST fallback");
    assert_eq!(prs.len(), 3);
    assert_eq!(prs[0].node_id, "PR_ready");
    assert_eq!(prs[0].fact.number, 42);
    assert_eq!(prs[0].fact.head_sha, "a".repeat(40));
    assert_eq!(prs[0].fact.head_branch, "feature");
    assert_eq!(prs[0].fact.labels, vec!["managed"]);
    assert!(prs[0].check_rollup_maybe_truncated);
    // The candidate's state comes from `pulls/{n}` and is arm-ready.
    assert!(crate::auto_arm::merge_state_is_arm_ready(
        &prs[0].fact.merge_state
    ));
    // Only the candidate is hydrated: not the draft, not the queued PR.
    let calls = fs::read_to_string(calls_path).expect("calls");
    assert!(calls.contains("repos/owner/repo/pulls/42"));
    assert!(!calls.contains("repos/owner/repo/pulls/43"));
    assert!(!calls.contains("repos/owner/repo/pulls/44"));
    assert!(!crate::auto_arm::merge_state_is_arm_ready(
        &prs[1].fact.merge_state
    ));
}

#[cfg(unix)]
#[test]
fn rest_fallback_refuses_a_candidate_whose_merge_state_is_unreadable() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"case "$*" in
  *"pr list"*) echo "HTTP 502 Bad Gateway" >&2; exit 1 ;;
  *"repos/owner/repo/pulls?state=open"*) printf '%s' '{REST_LIST_ROWS}' ;;
  *"repos/owner/repo/pulls/42") echo "HTTP 500" >&2; exit 1 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        ),
    );
    let prs = pull_requests(&actions, "owner/repo", "main", &BTreeMap::from([(44, 0)]))
        .expect("an unreadable state degrades to UNKNOWN, not a partial success");
    assert_eq!(prs[0].fact.merge_state, "UNKNOWN");
    assert!(!crate::auto_arm::merge_state_is_arm_ready(
        &prs[0].fact.merge_state
    ));
}

#[cfg(unix)]
#[test]
fn rest_fallback_refuses_a_candidate_whose_head_moved_before_the_state_read() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"case "$*" in
  *"pr list"*) echo "HTTP 502 Bad Gateway" >&2; exit 1 ;;
  *"repos/owner/repo/pulls?state=open"*) printf '%s' '{REST_LIST_ROWS}' ;;
  *"repos/owner/repo/pulls/42") printf '%s' '{{"mergeable_state":"clean","head":{{"sha":"dddddddddddddddddddddddddddddddddddddddd"}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        ),
    );
    let prs =
        pull_requests(&actions, "owner/repo", "main", &BTreeMap::from([(44, 0)])).expect("census");
    assert_eq!(prs[0].fact.merge_state, "UNKNOWN");
}

#[cfg(unix)]
#[test]
fn census_does_not_hydrate_drafts_conflicts_or_queued_pull_requests() {
    let temp = tempfile::tempdir().expect("temp");
    let calls_path = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"printf '%s\n' "$*" >> '{}'
case "$*" in
  *"/check-runs?per_page=100&page=1"*) printf '%s' '{{"check_runs":[]}}' ;;
  *"/statuses?per_page=100&page=1"*) printf '%s' '[]' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            calls_path.display()
        ),
    );
    let sha = |c: char| c.to_string().repeat(40);
    let row = |number: u64, c: char, draft: bool, state: &str| {
        serde_json::json!({
            "id": format!("PR_{number}"), "number": number, "isDraft": draft,
            "baseRefName": "main", "headRefOid": sha(c), "headRefName": format!("f{number}"),
            "author": {"login": "app/shipyard-local"}, "mergeStateStatus": state,
            "autoMergeRequest": null, "labels": []
        })
    };
    let positions = BTreeMap::from([(5, 0)]);
    let mut prs = [
        row(1, 'a', false, "CLEAN"),
        row(2, 'b', true, "CLEAN"),
        row(3, 'c', false, "DIRTY"),
        row(4, 'd', false, "CONFLICTING"),
        row(5, 'e', false, "CLEAN"),
        row(6, 'f', false, "BLOCKED"),
    ]
    .iter()
    .map(|row| parse_pr(row, &positions).expect("pr"))
    .collect::<Vec<_>>();
    hydrate_census_check_identities(&actions, "owner/repo", &[], &mut prs).expect("census");
    let calls = fs::read_to_string(calls_path).expect("calls");
    for hydrated in ['a', 'f'] {
        assert!(calls.contains(&format!("commits/{}/check-runs", sha(hydrated))));
    }
    for skipped in ['b', 'c', 'd', 'e'] {
        assert!(
            !calls.contains(&sha(skipped)),
            "{skipped} must not be hydrated"
        );
    }
    assert_eq!(
        calls.lines().count(),
        4,
        "two hydrated heads, two reads each"
    );
    assert!(!prs[0].check_rollup_maybe_truncated);
    assert!(prs[1].check_rollup_maybe_truncated, "left unobserved");
}

/// A labelled row is classified by its handoff commit status before the
/// queued, draft and conflict answers, so it must keep being hydrated.
#[cfg(unix)]
#[test]
fn census_keeps_the_handoff_status_for_labelled_queued_draft_and_dirty_rows() {
    use crate::merge_steward::{StewardDecision, StewardPolicy, classify_pr};
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"case "$*" in
  *"/check-runs?per_page=100&page=1"*) printf '%s' '{"check_runs":[]}' ;;
  *"/statuses?per_page=100&page=1"*) printf '%s' '[{"context":"shipyard/steward-handoff","state":"success","target_url":null,"created_at":"2026-07-26T02:00:00Z"}]' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let row = |number: u64, c: char, draft: bool, state: &str| {
        serde_json::json!({
            "id": format!("PR_{number}"), "number": number, "isDraft": draft,
            "baseRefName": "main", "headRefOid": c.to_string().repeat(40),
            "headRefName": format!("f{number}"), "author": {"login": "alice"},
            "mergeStateStatus": state, "autoMergeRequest": null,
            "labels": [{"name": "shipyard:managed"}]
        })
    };
    let positions = BTreeMap::from([(1, 0)]);
    let mut prs = [
        row(1, 'a', false, "CLEAN"),
        row(2, 'b', true, "CLEAN"),
        row(3, 'c', false, "DIRTY"),
    ]
    .iter()
    .map(|row| parse_pr(row, &positions).expect("pr"))
    .collect::<Vec<_>>();
    hydrate_census_check_identities(&actions, "owner/repo", &[], &mut prs).expect("census");
    let policy = StewardPolicy {
        merge_queue: true,
        native_auto_merge: true,
        approval_context: crate::merge_steward::DEFAULT_APPROVAL_CONTEXT.to_owned(),
        required_checks: Vec::new(),
        opt_out_label: "shipyard:unmanaged".to_owned(),
        provenance_blocking_labels: Vec::new(),
        managed_label: Some(MANAGED_LABEL.to_owned()),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 0,
    };
    let decisions = prs
        .iter()
        .map(|pr| classify_pr(&pr.fact, &policy, &BTreeMap::new()))
        .collect::<Vec<_>>();
    assert!(matches!(decisions[0], StewardDecision::Queued { .. }));
    assert_eq!(decisions[1], StewardDecision::Draft);
    assert!(matches!(decisions[2], StewardDecision::NeedsUpdate { .. }));
    for pr in &prs {
        assert!(
            crate::app::merge_steward_cmd::cancellation_revalidation::pull_request_is_managed(
                pr,
                MANAGED_LABEL,
                HANDOFF_CONTEXT
            )
        );
    }
}

#[cfg(unix)]
#[test]
fn census_fails_closed_when_a_candidates_checks_are_unreadable() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"case "$*" in
  *"commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/check-runs"*) echo "HTTP 500" >&2; exit 1 ;;
  *"/check-runs"*) printf '%s' '{"check_runs":[]}' ;;
  *"/statuses"*) printf '%s' '[]' ;;
  *) exit 2 ;;
esac
"#,
    );
    let row = |number: u64, c: char| {
        serde_json::json!({
            "id": format!("PR_{number}"), "number": number, "isDraft": false,
            "baseRefName": "main", "headRefOid": c.to_string().repeat(40),
            "headRefName": "f", "author": {"login": "app/shipyard-local"},
            "mergeStateStatus": "CLEAN", "autoMergeRequest": null, "labels": []
        })
    };
    let mut prs = ['a', 'b', 'c']
        .iter()
        .zip(1..)
        .map(|(c, n)| parse_pr(&row(n, *c), &BTreeMap::new()).expect("pr"))
        .collect::<Vec<_>>();
    let error = hydrate_census_check_identities(&actions, "owner/repo", &[], &mut prs)
        .expect_err("an unreadable candidate must fail the observation");
    assert!(error.contains("current-head check identities"), "{error}");
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
fn open_pull_request_transport_supports_more_than_one_hundred_rows() {
    let temp = tempfile::tempdir().expect("temp");
    let rows = (1..=101)
        .map(|number| {
            serde_json::json!({
                "id": format!("PR_{number}"),
                "number": number,
                "isDraft": false,
                "baseRefName": "main",
                "headRefOid": format!("{number:040x}"),
                "headRefName": format!("feature-{number}"),
                "mergeStateStatus": "CLEAN",
                "autoMergeRequest": null,
                "labels": [],
                "statusCheckRollup": []
            })
        })
        .collect::<Vec<_>>();
    let rows_path = temp.path().join("prs.json");
    fs::write(&rows_path, serde_json::to_vec(&rows).expect("rows JSON")).expect("write rows");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
test "$9" = --limit
test "${{10}}" = 200
cat '{}'
"#,
            rows_path.display()
        ),
    );

    let prs = pull_requests(&actions, "owner/repo", "main", &BTreeMap::new())
        .expect("complete open PR inventory");
    assert_eq!(prs.len(), 101);
    assert_eq!(prs.last().map(|pr| pr.fact.number), Some(101));
}

#[cfg(unix)]
#[test]
fn open_pull_request_transport_refuses_the_bounded_limit() {
    let temp = tempfile::tempdir().expect("temp");
    let rows = (1..=200)
        .map(|number| {
            serde_json::json!({
                "id": format!("PR_{number}"),
                "number": number,
                "isDraft": false,
                "baseRefName": "main",
                "headRefOid": format!("{number:040x}"),
                "headRefName": format!("feature-{number}"),
                "mergeStateStatus": "CLEAN",
                "autoMergeRequest": null,
                "labels": [],
                "statusCheckRollup": []
            })
        })
        .collect::<Vec<_>>();
    let rows_path = temp.path().join("prs.json");
    fs::write(&rows_path, serde_json::to_vec(&rows).expect("rows JSON")).expect("write rows");
    let actions = fake_gh(&temp, &format!("cat '{}'", rows_path.display()));

    let error = pull_requests(&actions, "owner/repo", "main", &BTreeMap::new())
        .expect_err("bounded inventory");
    assert!(error.contains("reached 200"), "{error}");
}
#[cfg(unix)]
#[test]
fn merge_queue_transport_paginates_beyond_one_hundred_entries() {
    let temp = tempfile::tempdir().expect("temp");
    let nodes = |range: std::ops::RangeInclusive<u64>| {
        range
            .map(|number| {
                serde_json::json!({
                    "position": number,
                    "enqueuedAt": "2026-09-01T00:00:00Z",
                    "headCommit": {"oid": format!("{number:040x}")},
                    "pullRequest": {"number": number}
                })
            })
            .collect::<Vec<_>>()
    };
    let first_page = temp.path().join("first.json");
    let second_page = temp.path().join("second.json");
    fs::write(
        &first_page,
        serde_json::to_vec(&serde_json::json!({
            "data": {"repository": {"mergeQueue": {"entries": {
                "nodes": nodes(1..=100),
                "pageInfo": {"hasNextPage": true, "endCursor": "cursor-100"}
            }}}}
        }))
        .expect("first page"),
    )
    .expect("write first page");
    fs::write(
        &second_page,
        serde_json::to_vec(&serde_json::json!({
            "data": {"repository": {"mergeQueue": {"entries": {
                "nodes": nodes(101..=102),
                "pageInfo": {"hasNextPage": false, "endCursor": "cursor-102"}
            }}}}
        }))
        .expect("second page"),
    )
    .expect("write second page");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
    if test "$#" -eq 10; then
  cat '{}'
    elif test "$#" -eq 12 && test "${{11}}" = -F && test "${{12}}" = cursor=cursor-100; then
  cat '{}'
else
  echo "unexpected pagination call: $*" >&2
  exit 2
fi
"#,
            first_page.display(),
            second_page.display()
        ),
    );

    let (enabled, positions, heads, enqueued) =
        merge_queue_snapshot(&actions, "owner/repo", "main").expect("complete snapshot");
    assert!(enabled);
    assert_eq!(positions.len(), 102);
    assert_eq!(positions.get(&101), Some(&101));
    assert_eq!(heads.get(&102), Some(&format!("{:040x}", 102)));
    assert_eq!(enqueued.len(), 102);
}

#[cfg(unix)]
#[test]
fn merge_queue_transport_refuses_page_overlap() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
if test "$#" -eq 10; then
  printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":1,"enqueuedAt":"2026-09-01T00:00:00Z","headCommit":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"pullRequest":{"number":42}}],"pageInfo":{"hasNextPage":true,"endCursor":"next"}}}}}}'
else
  printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":2,"enqueuedAt":"2026-09-01T00:00:01Z","headCommit":{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"pullRequest":{"number":42}}],"pageInfo":{"hasNextPage":false,"endCursor":"done"}}}}}}'
fi
"#,
    );

    let error = merge_queue_snapshot(&actions, "owner/repo", "main").expect_err("overlap");
    assert!(error.contains("repeated PR #42"), "{error}");
}

#[cfg(unix)]
#[test]
fn merge_queue_transport_refuses_movement_between_complete_reads() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
count_path=$(dirname "$0")/count
count=0
test ! -f "$count_path" || count=$(cat "$count_path")
count=$((count + 1))
printf '%s' "$count" > "$count_path"
case "$count" in
  1|3) printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":1,"enqueuedAt":"2026-09-01T00:00:00Z","headCommit":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"pullRequest":{"number":1}}],"pageInfo":{"hasNextPage":true,"endCursor":"next"}}}}}}' ;;
  2) printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":2,"enqueuedAt":"2026-09-01T00:00:01Z","headCommit":{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"pullRequest":{"number":2}}],"pageInfo":{"hasNextPage":false,"endCursor":"done"}}}}}}' ;;
  4) printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":2,"enqueuedAt":"2026-09-01T00:00:02Z","headCommit":{"oid":"cccccccccccccccccccccccccccccccccccccccc"},"pullRequest":{"number":3}}],"pageInfo":{"hasNextPage":false,"endCursor":"done"}}}}}}' ;;
  *) echo "unexpected pagination call $count: $*" >&2; exit 2 ;;
esac
"#,
    );

    let error = merge_queue_snapshot(&actions, "owner/repo", "main").expect_err("moving queue");
    assert!(error.contains("changed during pagination"), "{error}");
}

#[cfg(unix)]
#[test]
fn merge_queue_transport_refuses_malformed_page_info_and_cursor() {
    let cases = [
        (
            r#"{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{}}}}}}"#,
            "hasNextPage",
        ),
        (
            r#"{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":""}}}}}}"#,
            "non-empty endCursor",
        ),
    ];
    for (payload, expected) in cases {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(&temp, &format!("printf '%s' '{payload}'"));
        let error = merge_queue_snapshot(&actions, "owner/repo", "main").expect_err(expected);
        assert!(error.contains(expected), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn merge_queue_transport_refuses_repeated_cursor() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":"same"}}}}}}'"#,
    );

    let error = merge_queue_snapshot(&actions, "owner/repo", "main").expect_err("cursor cycle");
    assert!(error.contains("repeated endCursor"), "{error}");
}

#[cfg(unix)]
#[test]
fn merge_queue_transport_refuses_expired_deadline_without_github_call() {
    let temp = tempfile::tempdir().expect("temp");
    let called = temp.path().join("called");
    let actions = fake_gh(&temp, &format!(": > '{}'; exit 2", called.display()));

    let error = merge_queue_snapshot_before(
        &actions,
        "owner/repo",
        "main",
        Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("expired instant"),
    )
    .expect_err("expired deadline");
    assert!(error.contains("exceeded its bounded deadline"), "{error}");
    assert!(!called.exists(), "expired read must not invoke GitHub");
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
    printf '%s' '{"workflow_runs":[{"id":1,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":42}]}]}' ;;
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
fn active_run_transport_refuses_any_malformed_attempt_identity() {
    for attempt_field in ["", ",\"run_attempt\":0", ",\"run_attempt\":\"1\""] {
        let temp = tempfile::tempdir().expect("temp");
        let body = format!(
            r#"
case "$*" in
  *"actions/runs?status=queued"*)
    printf '%s' '{{"workflow_runs":[{{"id":1,"workflow_id":77{attempt_field},"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}]}}' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#
        );
        let actions = fake_gh(&temp, &body);
        assert!(active_runs(&actions, "owner/repo").is_err());
    }
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

#[cfg(unix)]
#[test]
fn open_pull_request_census_does_not_request_the_check_rollup() {
    let temp = tempfile::tempdir().expect("temp");
    let argv_path = temp.path().join("argv");
    let rows = serde_json::json!([{
        "id": "PR_1",
        "number": 1,
        "isDraft": false,
        "baseRefName": "main",
        "headRefOid": format!("{:040x}", 1),
        "headRefName": "feature-1",
        "mergeStateStatus": "CLEAN",
        "autoMergeRequest": null,
        "labels": []
    }]);
    let rows_path = temp.path().join("prs.json");
    fs::write(&rows_path, serde_json::to_vec(&rows).expect("rows JSON")).expect("write rows");
    let actions = fake_gh(
        &temp,
        &format!(
            "printf '%s' \"$*\" > '{}'\ncat '{}'\n",
            argv_path.display(),
            rows_path.display()
        ),
    );

    let prs = pull_requests(&actions, "owner/repo", "main", &BTreeMap::new()).expect("census");
    assert_eq!(prs.len(), 1);

    let argv = fs::read_to_string(&argv_path).expect("captured argv");
    // Control: the capture really observed the census argv, so the assertion
    // below is a measurement rather than an empty read.
    assert!(
        argv.contains("mergeStateStatus"),
        "argv capture observed nothing: {argv}"
    );
    assert!(
        !argv.contains("statusCheckRollup"),
        "the open-PR census must not request a per-PR check rollup: {argv}"
    );
}

#[test]
fn absent_check_rollup_marks_explicit_and_implicit_managed_pull_requests_for_hydration() {
    let positions = BTreeMap::new();
    let row = |number: u64, branch: &str, author: Option<&str>, labels: serde_json::Value| {
        let mut row = serde_json::json!({
            "id": format!("PR_{number}"),
            "number": number,
            "isDraft": false,
            "baseRefName": "main",
            "headRefOid": format!("{number:040x}"),
            "headRefName": branch,
            "mergeStateStatus": "CLEAN",
            "autoMergeRequest": null,
            "labels": labels
        });
        if let Some(author) = author {
            row["author"] = serde_json::json!({"login": author});
        }
        row
    };

    let managed = parse_pr(
        &row(
            1,
            "topic/managed",
            Some("alice"),
            serde_json::json!([{"name": "shipyard:managed"}]),
        ),
        &positions,
    )
    .expect("managed PR");
    let implicit = parse_pr(
        &row(
            2,
            "feature/fleet",
            Some("app/shipyard-local"),
            serde_json::json!([]),
        ),
        &positions,
    )
    .expect("implicit fleet PR");
    let ordinary = parse_pr(
        &row(3, "feature/human", Some("alice"), serde_json::json!([])),
        &positions,
    )
    .expect("ordinary PR");
    let foreign_app = parse_pr(
        &row(
            4,
            "feature/foreign-app",
            Some("app/other-automation"),
            serde_json::json!([]),
        ),
        &positions,
    )
    .expect("foreign App PR");

    assert!(
        managed.check_rollup_maybe_truncated,
        "a managed PR whose rollup was never observed must be hydrated per head"
    );
    assert!(
        implicit.check_rollup_maybe_truncated,
        "an unlabeled fleet PR is implicitly managed and must be hydrated per head"
    );
    assert!(
        !ordinary.check_rollup_maybe_truncated,
        "an ordinary human PR must not buy a per-head hydration round trip"
    );
    assert!(
        !foreign_app.check_rollup_maybe_truncated,
        "a non-fleet App must not buy a per-head hydration round trip"
    );
    assert!(managed.fact.checks.is_empty());
    assert!(implicit.fact.checks.is_empty());

    // An observed rollup keeps its existing meaning for the single-PR callers
    // that still request it: complete below the page cap, suspect at it.
    let mut observed = row(5, "feature/observed", Some("alice"), serde_json::json!([]));
    observed["statusCheckRollup"] = serde_json::json!([]);
    let observed = parse_pr(&observed, &positions).expect("observed rollup");
    assert!(!observed.check_rollup_maybe_truncated);
}

#[cfg(unix)]
#[test]
fn census_hydrates_unlabeled_fleet_checks_with_the_required_app_identity() {
    let temp = tempfile::tempdir().expect("temp");
    let calls_path = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"printf '%s\n' "$*" >> '{}'
case "$*" in
  *"/check-runs?per_page=100&page=1"*) printf '%s' '{{"check_runs":[{{"name":"macos","app":{{"id":42}},"status":"completed","conclusion":"success"}}]}}' ;;
  *"/statuses?per_page=100&page=1"*) printf '%s' '[]' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            calls_path.display()
        ),
    );
    let row = |number: u64, sha: &str, branch: &str, author: &str| {
        serde_json::json!({
            "id": format!("PR_{number}"),
            "number": number,
            "isDraft": false,
            "baseRefName": "main",
            "headRefOid": sha,
            "headRefName": branch,
            "author": {"login": author},
            "mergeStateStatus": "CLEAN",
            "autoMergeRequest": null,
            "labels": []
        })
    };
    let mut prs = vec![
        parse_pr(
            &row(1, &"a".repeat(40), "feature/fleet", "app/shipyard-local"),
            &BTreeMap::new(),
        )
        .expect("fleet PR"),
        parse_pr(
            &row(2, &"b".repeat(40), "feature/human", "alice"),
            &BTreeMap::new(),
        )
        .expect("human PR"),
        parse_pr(
            &row(
                3,
                &"c".repeat(40),
                "feature/foreign-app",
                "app/other-automation",
            ),
            &BTreeMap::new(),
        )
        .expect("foreign App PR"),
    ];
    hydrate_census_check_identities(
        &actions,
        "owner/repo",
        &[RequiredCheck {
            context: "macos".to_owned(),
            app_id: Some(42),
        }],
        &mut prs,
    )
    .expect("census hydration");

    assert_eq!(prs[0].fact.checks.len(), 1);
    assert_eq!(prs[0].fact.checks[0].app_id, Some(42));
    assert!(prs[1].fact.checks.is_empty());
    assert!(prs[2].fact.checks.is_empty());
    let calls = fs::read_to_string(calls_path).expect("hydration calls");
    assert!(calls.contains(&format!("commits/{}/check-runs", "a".repeat(40))));
    assert!(!calls.contains(&format!("commits/{}/check-runs", "b".repeat(40))));
    assert!(!calls.contains(&format!("commits/{}/check-runs", "c".repeat(40))));
}

/// The bulk open-PR read must stay within GitHub's per-query node budget no
/// matter how many pull requests are open. A nested connection on the list
/// (`commits` carries `authors`) multiplies per page and GitHub refuses the
/// whole query, which stopped every runner admission precheck.
#[test]
fn open_pr_list_fields_carry_no_nested_or_heavy_connections() {
    use crate::app::merge_steward_cmd::observation::OPEN_PR_LIST_FIELDS;
    let fields: Vec<&str> = OPEN_PR_LIST_FIELDS.split(',').collect();
    for forbidden in [
        "commits",
        "statusCheckRollup",
        "comments",
        "reviews",
        "latestReviews",
        "reviewRequests",
        "files",
        "closingIssuesReferences",
        "assignees",
        "projectItems",
    ] {
        assert!(
            !fields.contains(&forbidden),
            "{forbidden} must not be read for every open PR: {OPEN_PR_LIST_FIELDS}"
        );
    }
    assert_eq!(
        fields,
        [
            "id",
            "number",
            "isDraft",
            "baseRefName",
            "headRefOid",
            "headRefName",
            "mergeStateStatus",
            "autoMergeRequest",
            "labels",
            "author",
        ]
    );
}

#[cfg(unix)]
#[test]
fn bulk_pull_request_read_requests_the_pinned_fields_and_leaves_owner_open() {
    use crate::app::merge_steward_cmd::observation::{OPEN_PR_LIST_FIELDS, pull_requests};
    let temp = tempfile::tempdir().expect("temp");
    let root = temp.path().display().to_string();
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
case "$*" in
  "pr list "*) printf '%s\n' "$*" >> '{root}/list-args'; printf '%s' '[{{"id":"PR_a","number":1,"isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"a","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"author":{{"login":"alice"}}}},{{"id":"PR_b","number":2,"isDraft":false,"baseRefName":"main","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","headRefName":"b","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"author":{{"login":"bob"}}}}]' ;;
  *) printf '%s' '{{}}' ;;
esac
"#
        ),
    );
    let prs = pull_requests(&actions, "owner/repo", "main", &BTreeMap::new()).expect("PRs");
    let args = fs::read_to_string(temp.path().join("list-args")).expect("list args");
    assert_eq!(args.lines().count(), 1, "{args}");
    assert!(
        args.trim_end()
            .ends_with(&format!("--json {OPEN_PR_LIST_FIELDS}")),
        "{args}"
    );
    assert!(!args.contains("commits"), "{args}");
    assert!(!args.contains("body"), "{args}");
    assert_eq!(prs.len(), 2);
    assert_eq!(prs[0].owner, None, "no whence and no commits: undecided");
    assert_eq!(prs[0].fact.author.as_deref(), Some("alice"));
    assert_eq!(prs[1].owner, None, "owner is resolved lazily from PR view");
}

/// A fake repository answering only the reads `shipyard pr`'s next-steps block
/// makes: settings, evaluated rules, classic protection, and the merge queue.
#[cfg(unix)]
fn next_steps_for(
    temp: &tempfile::TempDir,
    queue: &str,
    rules: &str,
) -> crate::pr_next_steps::NextSteps {
    let actions = fake_gh(
        temp,
        &format!(
            r#"
case "$*" in
  "api repos/owner/repo") printf '%s' '{{"full_name":"owner/repo","allow_auto_merge":true}}' ;;
  *"rules/branches/main --paginate --slurp"*) printf '%s' '{rules}' ;;
  *"branches/main/protection/required_status_checks"*) echo "HTTP 404" >&2; exit 1 ;;
  "api graphql "*) printf '%s' '{queue}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#
        ),
    );
    observe_next_steps(&actions, "owner/repo", "main")
}

#[cfg(unix)]
#[test]
fn next_steps_reads_a_native_queue_repository() {
    let temp = tempfile::tempdir().expect("temp");
    let steps = next_steps_for(
        &temp,
        r#"{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#,
        r#"[[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"macos"}]}}]]"#,
    );
    let lines = steps.lines();
    assert!(lines[1].contains("enqueues the approved head into the merge queue"));
    assert_eq!(steps.to_json()["allow_auto_merge"], true);
    assert_eq!(steps.to_json()["merge_queue"], true);
}

#[cfg(unix)]
#[test]
fn next_steps_reads_an_enforced_approval_repository_without_a_queue() {
    let temp = tempfile::tempdir().expect("temp");
    let steps = next_steps_for(
        &temp,
        r#"{"data":{"repository":{"mergeQueue":null}}}"#,
        r#"[[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"macos"},{"context":"review-verdict","integration_id":42}]}}]]"#,
    );
    assert!(steps.lines()[1].contains("merges the approved head exactly; do not merge"));
    assert_eq!(steps.to_json()["merge_queue"], false);
}

#[cfg(unix)]
#[test]
fn next_steps_reads_a_repository_with_no_automatic_admission() {
    let temp = tempfile::tempdir().expect("temp");
    let steps = next_steps_for(
        &temp,
        r#"{"data":{"repository":{"mergeQueue":null}}}"#,
        r#"[[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"macos"}]}}]]"#,
    );
    assert!(steps.lines()[1].starts_with("No automatic admission exists"));
}

#[cfg(unix)]
#[test]
fn next_steps_claims_nothing_when_settings_cannot_be_read() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, "echo boom >&2; exit 1");
    let steps = observe_next_steps(&actions, "owner/repo", "main");
    assert!(steps.lines()[1].contains("could not be read"));
    assert!(steps.to_json()["unavailable_reason"].is_string());
}
