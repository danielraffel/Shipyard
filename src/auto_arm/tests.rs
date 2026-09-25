use super::{
    ArmSkip, ArmVerdict, NATIVE_AUTO_MERGE_MUTATION, arm_mutation_args, arm_response_accepted,
    decide_from_queue_state, first_graphql_error, is_arm_guard_refusal,
    is_auto_merge_disabled_refusal, merge_state_is_arm_ready, preselect_backstop_candidate,
};
use crate::merge_steward::StewardPullRequest;
use crate::pr_queue_state::PrQueueState;

fn open_pr() -> StewardPullRequest {
    StewardPullRequest {
        number: 7,
        head_sha: "a".repeat(40),
        head_branch: "feature/x".to_owned(),
        draft: false,
        merge_state: "CLEAN".to_owned(),
        auto_merge_active: false,
        queue_position: None,
        labels: Vec::new(),
        checks: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The mutation itself
// ---------------------------------------------------------------------------

/// A squash subject folds the `chore: bump versions` marker commit into itself,
/// which trips release automation's "was a release cut" reader. The merge
/// method is the whole reason this constant is pinned rather than configurable.
#[test]
fn arm_mutation_always_merges_and_never_squashes() {
    assert!(NATIVE_AUTO_MERGE_MUTATION.contains("mergeMethod:MERGE"));
    let lowered = NATIVE_AUTO_MERGE_MUTATION.to_ascii_lowercase();
    assert!(!lowered.contains("squash"), "{NATIVE_AUTO_MERGE_MUTATION}");
    assert!(!lowered.contains("rebase"), "{NATIVE_AUTO_MERGE_MUTATION}");
}

#[test]
fn arm_mutation_is_the_native_auto_merge_mutation() {
    assert!(NATIVE_AUTO_MERGE_MUTATION.contains("enablePullRequestAutoMerge"));
    // Native auto-merge, not a direct enqueue: the point is a server-owned
    // request that outlives every local process.
    assert!(!NATIVE_AUTO_MERGE_MUTATION.contains("enqueuePullRequest"));
}

#[test]
fn arm_mutation_args_bind_the_node_id_as_a_variable() {
    let args = arm_mutation_args("PR_kwDOABCD");
    assert_eq!(args[0], "api");
    assert_eq!(args[1], "graphql");
    assert!(args.iter().any(|arg| arg == "id=PR_kwDOABCD"));
    // Interpolating the id into the document instead of binding it would make
    // a hostile node id a query fragment.
    assert!(
        args.iter()
            .all(|arg| !arg.starts_with("query=") || !arg.contains("PR_kwDOABCD"))
    );
}

// ---------------------------------------------------------------------------
// Reading the mutation response
// ---------------------------------------------------------------------------

#[test]
fn a_response_carrying_the_armed_pull_request_is_accepted() {
    assert!(arm_response_accepted(
        r#"{"data":{"enablePullRequestAutoMerge":{"pullRequest":{"number":7}}}}"#
    ));
}

/// GitHub answers a PARTIAL failure with HTTP 200, the mutation payload, AND an
/// `errors` array. Without the errors check this reads as a clean success, so
/// this case is the only one that makes that check load-bearing.
#[test]
fn a_partial_success_carrying_both_a_payload_and_errors_is_not_accepted() {
    assert!(!arm_response_accepted(
        r#"{"data":{"enablePullRequestAutoMerge":{"pullRequest":{"number":7}}},
            "errors":[{"message":"Something went wrong while executing your query"}]}"#
    ));
}

#[test]
fn a_null_payload_is_not_accepted() {
    assert!(!arm_response_accepted(
        r#"{"data":{"enablePullRequestAutoMerge":null}}"#
    ));
}

#[test]
fn a_response_without_the_mutation_payload_is_not_accepted() {
    assert!(!arm_response_accepted(r#"{"data":{}}"#));
    assert!(!arm_response_accepted("not json"));
}

#[test]
fn the_first_graphql_error_is_reported_for_a_rejection() {
    assert_eq!(
        first_graphql_error(
            r#"{"errors":[{"message":"Pull request is in clean status"},
                          {"message":"second"}]}"#
        )
        .as_deref(),
        Some("Pull request is in clean status")
    );
    assert_eq!(first_graphql_error(r#"{"data":{}}"#), None);
}

// ---------------------------------------------------------------------------
// merge-state readiness
// ---------------------------------------------------------------------------

#[test]
fn behind_is_arm_ready_because_a_queue_absorbs_it() {
    assert!(merge_state_is_arm_ready("BEHIND"));
    assert!(merge_state_is_arm_ready("behind"));
}

#[test]
fn clean_and_unstable_are_arm_ready() {
    assert!(merge_state_is_arm_ready("CLEAN"));
    // UNSTABLE is a failing NON-required check; GitHub still merges it.
    assert!(merge_state_is_arm_ready("UNSTABLE"));
    assert!(merge_state_is_arm_ready("HAS_HOOKS"));
}

#[test]
fn blocked_and_dirty_and_unknown_are_not_arm_ready() {
    // BLOCKED is the required-checks-failing / review-outstanding state.
    assert!(!merge_state_is_arm_ready("BLOCKED"));
    assert!(!merge_state_is_arm_ready("DIRTY"));
    assert!(!merge_state_is_arm_ready("CONFLICTING"));
    // GitHub still computing mergeability is not permission.
    assert!(!merge_state_is_arm_ready("UNKNOWN"));
    assert!(!merge_state_is_arm_ready(""));
}

// ---------------------------------------------------------------------------
// decide_from_queue_state — the arm-on-open path
// ---------------------------------------------------------------------------

#[test]
fn a_never_armed_open_pr_is_armed() {
    assert_eq!(
        decide_from_queue_state(&PrQueueState::NeverArmed, false),
        ArmVerdict::Arm
    );
}

#[test]
fn a_never_armed_draft_is_not_armed() {
    assert_eq!(
        decide_from_queue_state(&PrQueueState::NeverArmed, true),
        ArmVerdict::Skip(ArmSkip::Draft)
    );
}

/// The REST/GraphQL `auto_merge == null` trap: GitHub consumes the auto-merge
/// request on admission, so a queued PR reads as unarmed to a naive reader.
/// Re-arming it re-enqueues a head the queue already holds.
#[test]
fn a_queued_pr_is_never_re_armed() {
    let verdict = decide_from_queue_state(
        &PrQueueState::Queued {
            entry_state: Some("AWAITING_CHECKS".to_owned()),
            position: Some(3),
            requeues_without_new_head: 0,
        },
        false,
    );
    assert_eq!(
        verdict,
        ArmVerdict::Skip(ArmSkip::AlreadyQueued { position: Some(3) })
    );
}

#[test]
fn an_already_armed_pr_is_left_alone_so_the_pass_is_idempotent() {
    let verdict = decide_from_queue_state(
        &PrQueueState::ArmedNotQueued {
            enabled_at: Some("2026-09-25T00:00:00Z".to_owned()),
            requeues_without_new_head: 0,
        },
        false,
    );
    assert!(!verdict.arms());
    assert!(matches!(
        verdict,
        ArmVerdict::Skip(ArmSkip::AlreadyArmed { .. })
    ));
}

#[test]
fn an_ejected_pr_on_the_same_head_is_not_re_armed() {
    let verdict = decide_from_queue_state(
        &PrQueueState::Ejected {
            reason: "failed_checks".to_owned(),
            at: Some("2026-09-25T00:00:00Z".to_owned()),
            new_head_since_removal: false,
            requeues_without_new_head: 1,
        },
        false,
    );
    assert_eq!(
        verdict,
        ArmVerdict::Skip(ArmSkip::EjectedSameHead {
            reason: "failed_checks".to_owned()
        })
    );
}

#[test]
fn an_ejected_pr_with_a_new_head_is_armed() {
    let verdict = decide_from_queue_state(
        &PrQueueState::Ejected {
            reason: "failed_checks".to_owned(),
            at: Some("2026-09-25T00:00:00Z".to_owned()),
            new_head_since_removal: true,
            requeues_without_new_head: 0,
        },
        false,
    );
    assert_eq!(verdict, ArmVerdict::Arm);
}

/// `invalid_merge_commit` is GitHub failing to build the merge commit, which
/// says nothing against the head — the one same-head re-arm the repo allows.
#[test]
fn an_ejected_pr_removed_for_invalid_merge_commit_is_armed_on_the_same_head() {
    let verdict = decide_from_queue_state(
        &PrQueueState::Ejected {
            reason: "invalid_merge_commit".to_owned(),
            at: None,
            new_head_since_removal: false,
            requeues_without_new_head: 0,
        },
        false,
    );
    assert_eq!(verdict, ArmVerdict::Arm);
}

#[test]
fn an_ejected_draft_is_declined_as_a_draft_not_armed() {
    let verdict = decide_from_queue_state(
        &PrQueueState::Ejected {
            reason: "invalid_merge_commit".to_owned(),
            at: None,
            new_head_since_removal: true,
            requeues_without_new_head: 0,
        },
        true,
    );
    assert_eq!(verdict, ArmVerdict::Skip(ArmSkip::Draft));
}

#[test]
fn merged_and_closed_are_left_alone() {
    assert_eq!(
        decide_from_queue_state(&PrQueueState::Merged, false),
        ArmVerdict::Skip(ArmSkip::Merged)
    );
    assert_eq!(
        decide_from_queue_state(&PrQueueState::Closed, false),
        ArmVerdict::Skip(ArmSkip::Closed)
    );
}

/// An unreadable state is not an unarmed state. Arming blind here is how a
/// queued PR gets re-enqueued when the read merely failed.
#[test]
fn an_unknown_state_is_never_armed() {
    let verdict = decide_from_queue_state(
        &PrQueueState::Unknown {
            detail: "isInMergeQueue is missing".to_owned(),
        },
        false,
    );
    assert!(!verdict.arms());
    assert!(matches!(verdict, ArmVerdict::Skip(ArmSkip::Unknown { .. })));
}

// ---------------------------------------------------------------------------
// preselect_backstop_candidate — the cheap periodic pre-filter
// ---------------------------------------------------------------------------

#[test]
fn a_clean_unarmed_unqueued_open_pr_is_a_backstop_candidate() {
    assert_eq!(
        preselect_backstop_candidate(&open_pr(), true, "shipyard:no-auto-merge"),
        ArmVerdict::Arm
    );
}

#[test]
fn a_repo_without_native_auto_merge_yields_no_candidates() {
    assert_eq!(
        preselect_backstop_candidate(&open_pr(), false, "shipyard:no-auto-merge"),
        ArmVerdict::Skip(ArmSkip::NativeAutoMergeDisabled)
    );
}

#[test]
fn the_opt_out_label_excludes_a_pr_case_insensitively() {
    let mut pr = open_pr();
    pr.labels = vec!["Shipyard:No-Auto-Merge".to_owned()];
    let verdict = preselect_backstop_candidate(&pr, true, "shipyard:no-auto-merge");
    assert!(matches!(
        verdict,
        ArmVerdict::Skip(ArmSkip::OptedOut { .. })
    ));
}

#[test]
fn a_queued_row_is_excluded_even_though_its_auto_merge_flag_is_false() {
    let mut pr = open_pr();
    // Exactly the shape GitHub returns for a queued PR: auto-merge consumed on
    // admission, so the flag is false while the PR is very much in the queue.
    pr.auto_merge_active = false;
    pr.queue_position = Some(0);
    assert_eq!(
        preselect_backstop_candidate(&pr, true, "shipyard:no-auto-merge"),
        ArmVerdict::Skip(ArmSkip::AlreadyQueued { position: Some(0) })
    );
}

#[test]
fn an_already_armed_row_is_excluded() {
    let mut pr = open_pr();
    pr.auto_merge_active = true;
    assert!(matches!(
        preselect_backstop_candidate(&pr, true, "shipyard:no-auto-merge"),
        ArmVerdict::Skip(ArmSkip::AlreadyArmed { .. })
    ));
}

#[test]
fn a_draft_row_is_excluded() {
    let mut pr = open_pr();
    pr.draft = true;
    assert_eq!(
        preselect_backstop_candidate(&pr, true, "shipyard:no-auto-merge"),
        ArmVerdict::Skip(ArmSkip::Draft)
    );
}

#[test]
fn a_blocked_row_is_excluded_because_a_required_check_is_not_passing() {
    let mut pr = open_pr();
    pr.merge_state = "BLOCKED".to_owned();
    assert_eq!(
        preselect_backstop_candidate(&pr, true, "shipyard:no-auto-merge"),
        ArmVerdict::Skip(ArmSkip::NotArmReady {
            merge_state: "BLOCKED".to_owned()
        })
    );
}

#[test]
fn a_dirty_row_is_excluded() {
    let mut pr = open_pr();
    pr.merge_state = "DIRTY".to_owned();
    assert!(matches!(
        preselect_backstop_candidate(&pr, true, "shipyard:no-auto-merge"),
        ArmVerdict::Skip(ArmSkip::NotArmReady { .. })
    ));
}

#[test]
fn a_behind_row_is_a_candidate_because_the_queue_absorbs_it() {
    let mut pr = open_pr();
    pr.merge_state = "BEHIND".to_owned();
    assert_eq!(
        preselect_backstop_candidate(&pr, true, "shipyard:no-auto-merge"),
        ArmVerdict::Arm
    );
}

// ---------------------------------------------------------------------------
// Guard agreement
// ---------------------------------------------------------------------------

#[test]
fn a_guard_refusal_is_recognised_from_its_stderr() {
    assert!(is_arm_guard_refusal(
        "queue-arm-guard: refusing: PR #12 is already in the merge queue at position 1."
    ));
    assert!(is_arm_guard_refusal(
        "gh exited 1: queue-arm-guard: refusing ambiguous queue-arm request: no targets"
    ));
}

#[test]
fn an_ordinary_github_failure_is_not_read_as_a_guard_refusal() {
    assert!(!is_arm_guard_refusal("HTTP 502: Bad Gateway"));
    assert!(!is_arm_guard_refusal(
        "GraphQL: Resource not accessible by integration"
    ));
}

/// Every state the Python arm guard refuses must be a state this module also
/// refuses, or Shipyard would issue a request the guard then rejects, turning
/// a normal no-op into a reported failure on every pass.
#[test]
fn every_state_the_guard_refuses_is_also_refused_here() {
    let guard_refuses = [
        PrQueueState::Queued {
            entry_state: None,
            position: Some(1),
            requeues_without_new_head: 0,
        },
        PrQueueState::ArmedNotQueued {
            enabled_at: None,
            requeues_without_new_head: 0,
        },
        PrQueueState::Ejected {
            reason: "failed_checks".to_owned(),
            at: None,
            new_head_since_removal: false,
            requeues_without_new_head: 0,
        },
        PrQueueState::Ejected {
            reason: "merge_conflict".to_owned(),
            at: None,
            new_head_since_removal: false,
            requeues_without_new_head: 0,
        },
        PrQueueState::Ejected {
            reason: "dequeued_by_a_human".to_owned(),
            at: None,
            new_head_since_removal: false,
            requeues_without_new_head: 0,
        },
        PrQueueState::Merged,
        PrQueueState::Closed,
        PrQueueState::Unknown {
            detail: "unreadable".to_owned(),
        },
    ];
    for state in &guard_refuses {
        assert!(
            !decide_from_queue_state(state, false).arms(),
            "state {state:?} must not be armed: the arm guard refuses it"
        );
    }
}

/// The two states the guard explicitly allows must be armed here, or the
/// backstop would never fire and its green run would prove nothing.
#[test]
fn both_states_the_guard_allows_are_armed_here() {
    assert!(decide_from_queue_state(&PrQueueState::NeverArmed, false).arms());
    assert!(
        decide_from_queue_state(
            &PrQueueState::Ejected {
                reason: "failed_checks".to_owned(),
                at: None,
                new_head_since_removal: true,
                requeues_without_new_head: 0,
            },
            false
        )
        .arms()
    );
}

#[test]
fn skip_reasons_explain_themselves_without_naming_an_override() {
    let skips = [
        ArmSkip::AlreadyArmed { enabled_at: None },
        ArmSkip::AlreadyQueued { position: Some(2) },
        ArmSkip::Draft,
        ArmSkip::Merged,
        ArmSkip::Closed,
        ArmSkip::EjectedSameHead {
            reason: "failed_checks".to_owned(),
        },
        ArmSkip::NotArmReady {
            merge_state: "BLOCKED".to_owned(),
        },
        ArmSkip::OptedOut {
            label: "shipyard:no-auto-merge".to_owned(),
        },
        ArmSkip::NativeAutoMergeDisabled,
        ArmSkip::Unknown {
            detail: "x".to_owned(),
        },
    ];
    for skip in &skips {
        let text = skip.explain();
        assert!(!text.is_empty(), "{skip:?}");
        // An explanation that names the override teaches an agent to set it.
        assert!(
            !text.contains("GHAPP_ALLOW_QUEUE_REARM"),
            "{skip:?} names the operator override"
        );
    }
}

// ---------------------------------------------------------------------------
// "this repository does not allow auto-merge" is a setting, not a fault
// ---------------------------------------------------------------------------

/// Observed live: `danielraffel/Shipyard` itself has `allow_auto_merge=false`.
/// Reporting that as a warning on every single ship would teach a reader to
/// ignore the warning that matters, so it must read as an ordinary no-op.
#[test]
fn githubs_auto_merge_disabled_wordings_are_recognised() {
    for message in [
        "GraphQL: Auto merge is not allowed for this repository (enablePullRequestAutoMerge)",
        "Can't enable auto-merge for this pull request. Please ensure auto-merge is \
         enabled in repository settings.",
        "AUTO-MERGE IS NOT ENABLED FOR THIS REPOSITORY",
        "auto merge is disabled",
    ] {
        assert!(
            is_auto_merge_disabled_refusal(message),
            "not recognised: {message}"
        );
    }
}

#[test]
fn an_unrelated_failure_is_not_read_as_auto_merge_being_disabled() {
    for message in [
        "HTTP 502: Bad Gateway",
        "GraphQL: Resource not accessible by integration",
        // A guard refusal names the queue, not the repository setting: these two
        // recognisers must not both claim the same message.
        "queue-arm-guard: refusing: PR #7 is already in the merge queue",
        // "not allowed" alone is not enough; it must be about auto-merge.
        "Pushing to this branch is not allowed",
    ] {
        assert!(
            !is_auto_merge_disabled_refusal(message),
            "wrongly recognised: {message}"
        );
    }
}
