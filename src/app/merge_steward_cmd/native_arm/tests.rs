use std::collections::BTreeMap;

use super::{apply_native_arm_backstop, steward_declines_ownership};
use crate::app::merge_steward_cmd::{ObservedPr, PrReport, RepoObservation, parse_pr};
use crate::cloud::GitHubActions;
use crate::merge_steward::{CapacityPreemptionPolicy, RequiredCheck, StewardDecision};

/// A `gh` stand-in: one shell script that answers the queue-state read and the
/// arm mutation by matching on its own argv, and appends every call to a log.
#[cfg(unix)]
fn fake_gh(temp: &tempfile::TempDir, body: &str) -> GitHubActions {
    let path = temp.path().join("gh");
    crate::test_support::write_executable_script(&path, &format!("#!/bin/sh\nset -eu\n{body}\n"));
    let config = crate::config::LoadedConfig {
        data: toml::Table::new(),
        global_dir: temp.path().join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    };
    GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(path)
}

/// Script that logs argv, then answers a never-armed state and an accepted arm.
#[cfg(unix)]
fn never_armed_then_arms(log: &str) -> String {
    format!(
        r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *enablePullRequestAutoMerge*)
    printf '%s' '{{"data":{{"enablePullRequestAutoMerge":{{"pullRequest":{{"number":42}}}}}}}}' ;;
  *isInMergeQueue*)
    printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *) printf '%s' '{{}}' ;;
esac"#
    )
}

/// Script whose queue-state read reports the PR as ejected on this exact head.
#[cfg(unix)]
fn ejected_same_head(log: &str) -> String {
    format!(
        r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *isInMergeQueue*)
    printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[{{"__typename":"AddedToMergeQueueEvent","createdAt":"2026-09-01T00:00:00Z","actor":{{"login":"a"}}}},{{"__typename":"RemovedFromMergeQueueEvent","createdAt":"2026-09-02T00:00:00Z","reason":"failed_checks","actor":{{"login":"a"}}}}]}}}}}}}}}}' ;;
  *) printf '%s' '{{}}' ;;
esac"#
    )
}

fn pr_row(overrides: &serde_json::Value) -> ObservedPr {
    let mut row = serde_json::json!({
        "id": "PR_node42",
        "number": 42,
        "state": "OPEN",
        "isDraft": false,
        "baseRefName": "main",
        "headRefOid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "headRefName": "feature",
        "mergeStateStatus": "CLEAN",
        "autoMergeRequest": null,
        "labels": [],
    });
    if let (Some(base), Some(extra)) = (row.as_object_mut(), overrides.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    parse_pr(&row, &BTreeMap::new()).expect("PR row")
}

fn observation(pr: ObservedPr, allow_auto_merge: bool) -> RepoObservation {
    RepoObservation {
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        allow_auto_merge,
        merge_queue: true,
        required_checks: vec![RequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
        }],
        prs: vec![pr],
        runs: Vec::new(),
        merge_group_heads: BTreeMap::new(),
        merge_group_enqueued_at: BTreeMap::new(),
        capacity_preemption_policy: CapacityPreemptionPolicy::pulp(),
        preemption_error: None,
    }
}

fn report(decision: StewardDecision) -> Vec<PrReport> {
    vec![PrReport {
        number: 42,
        head_sha: "a".repeat(40),
        decision,
        mutation: None,
        error: None,
    }]
}

// ---------------------------------------------------------------------------
// Ownership split — the backstop must never contend with the enqueue path
// ---------------------------------------------------------------------------

#[test]
fn only_unmanaged_and_handoff_missing_are_the_backstops_business() {
    assert!(steward_declines_ownership(&StewardDecision::Unmanaged));
    assert!(steward_declines_ownership(&StewardDecision::HandoffMissing));
    // Everything else means the managed path owns the PR: it is acting now, or
    // deliberately waiting. Arming underneath it would contend with it.
    for owned in [
        StewardDecision::ArmMergeQueue,
        StewardDecision::Queued { position: 0 },
        StewardDecision::OptedOut,
        StewardDecision::Draft,
        StewardDecision::InvalidHead,
        StewardDecision::WaitingRequired {
            contexts: vec!["macos".to_owned()],
        },
        StewardDecision::RequiredFailed {
            contexts: vec!["macos".to_owned()],
        },
        StewardDecision::RerunTransient { run_ids: vec![1] },
        StewardDecision::ProvenanceBlocked {
            labels: vec!["5·unresolved".to_owned()],
        },
        StewardDecision::NeedsUpdate {
            merge_state: "DIRTY".to_owned(),
        },
    ] {
        assert!(
            !steward_declines_ownership(&owned),
            "{owned:?} is owned by the managed path"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_pr_the_steward_will_enqueue_is_never_armed_by_the_backstop() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(&temp, &never_armed_then_arms(&log.display().to_string()));
    let observation = observation(pr_row(&serde_json::json!({})), true);
    let (status, unhealthy) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::ArmMergeQueue),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(!unhealthy);
    assert!(status.results.is_empty(), "{status:?}");
    assert!(status.policy.contains("no candidates"));
    // Not one GitHub call: the candidate filter is free.
    assert!(!log.exists(), "the backstop read GitHub for a managed PR");
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn an_unmanaged_green_unarmed_pr_is_armed_with_merge() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(&temp, &never_armed_then_arms(&log.display().to_string()));
    let observation = observation(pr_row(&serde_json::json!({})), true);
    let (status, unhealthy) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(!unhealthy, "{status:?}");
    assert_eq!(status.results.len(), 1);
    assert_eq!(status.results[0].outcome, "armed");
    assert_eq!(status.results[0].number, 42);
    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(calls.contains("mergeMethod:MERGE"), "{calls}");
    assert!(calls.contains("id=PR_node42"), "{calls}");
    // Never a squash: it folds the version-bump marker commit in.
    assert!(!calls.to_ascii_lowercase().contains("squash"), "{calls}");
}

/// Audit mode is the default for the steward, and it must mutate nothing.
#[cfg(unix)]
#[test]
fn without_apply_the_backstop_reports_would_arm_and_mutates_nothing() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(&temp, &never_armed_then_arms(&log.display().to_string()));
    let observation = observation(pr_row(&serde_json::json!({})), true);
    let (status, unhealthy) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        false,
    );
    assert!(!unhealthy);
    assert_eq!(status.results[0].outcome, "would_arm");
    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(
        !calls.contains("enablePullRequestAutoMerge"),
        "audit mode issued a mutation: {calls}"
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// The reason the cheap row is not enough: it carries no timeline, so it cannot
/// tell never-armed from ejected-on-this-head. Re-arming the latter re-enqueues
/// it, and under ALLGREEN that fails every batch-mate with it.
#[cfg(unix)]
#[test]
fn a_candidate_the_queue_ejected_on_this_head_is_confirmed_and_refused() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(&temp, &ejected_same_head(&log.display().to_string()));
    let observation = observation(pr_row(&serde_json::json!({})), true);
    let (status, unhealthy) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(!unhealthy, "an ejection is a normal outcome: {status:?}");
    assert_eq!(status.results[0].outcome, "skipped");
    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(
        !calls.contains("enablePullRequestAutoMerge"),
        "re-armed an ejected head: {calls}"
    );
}

#[cfg(unix)]
#[test]
fn a_draft_is_filtered_before_any_github_read() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(&temp, &never_armed_then_arms(&log.display().to_string()));
    let observation = observation(pr_row(&serde_json::json!({"isDraft": true})), true);
    let (status, _) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(status.results.is_empty());
    assert!(!log.exists(), "read GitHub for a draft");
}

#[cfg(unix)]
#[test]
fn a_pr_whose_required_checks_are_failing_is_filtered_before_any_github_read() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(&temp, &never_armed_then_arms(&log.display().to_string()));
    // BLOCKED is GitHub's own verdict for a failing/missing required check.
    let observation = observation(pr_row(&serde_json::json!({"mergeStateStatus": "BLOCKED"})), true);
    let (status, _) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(status.results.is_empty(), "{status:?}");
    assert!(!log.exists(), "read GitHub for a BLOCKED PR");
}

#[cfg(unix)]
#[test]
fn an_already_armed_pr_is_filtered_so_repeated_passes_are_idempotent() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(&temp, &never_armed_then_arms(&log.display().to_string()));
    let observation = observation(
        pr_row(&serde_json::json!({"autoMergeRequest": {"enabledAt": "2026-09-25T00:00:00Z"}})),
        true,
    );
    let (status, _) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(status.results.is_empty(), "{status:?}");
    assert!(!log.exists(), "read GitHub for an already-armed PR");
}

#[cfg(unix)]
#[test]
fn the_opt_out_label_excludes_a_pr_from_the_backstop() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(&temp, &never_armed_then_arms(&log.display().to_string()));
    let observation = observation(
        pr_row(&serde_json::json!({"labels": [{"name": "shipyard:no-auto-merge"}]})),
        true,
    );
    let (status, _) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(status.results.is_empty(), "{status:?}");
    assert!(!log.exists());
}

#[cfg(unix)]
#[test]
fn a_repo_without_native_auto_merge_declines_the_whole_pass() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(&temp, &never_armed_then_arms(&log.display().to_string()));
    let observation = observation(pr_row(&serde_json::json!({})), false);
    let (status, unhealthy) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(!unhealthy);
    assert!(status.policy.contains("does not allow native auto-merge"));
    assert!(status.results.is_empty());
    assert!(!log.exists());
}

/// An unreadable state is not an unarmed state, and it is worth a nonzero exit:
/// the pass could not do its job.
#[cfg(unix)]
#[test]
fn an_unreadable_queue_state_arms_nothing_and_is_reported_unhealthy() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"printf '%s\n' "$*" >> '{}'
case "$*" in
  *isInMergeQueue*) echo 'HTTP 502' >&2; exit 1 ;;
  *) printf '%s' '{{}}' ;;
esac"#,
            log.display()
        ),
    );
    let observation = observation(pr_row(&serde_json::json!({})), true);
    let (status, unhealthy) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(unhealthy, "{status:?}");
    assert_eq!(status.results[0].outcome, "skipped");
    assert!(
        status.results[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("refusing to arm blind")),
        "{status:?}"
    );
    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(!calls.contains("enablePullRequestAutoMerge"), "{calls}");
}

/// The guard refuses exactly the states this pass refuses, so its refusal is
/// agreement — reported, not a failure, and never overridden.
#[cfg(unix)]
#[test]
fn a_queue_arm_guard_refusal_is_agreement_not_a_failure() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"printf '%s\n' "$*" >> '{}'
case "$*" in
  *enablePullRequestAutoMerge*)
    echo 'queue-arm-guard: refusing: PR #42 is already in the merge queue' >&2; exit 1 ;;
  *isInMergeQueue*)
    printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *) printf '%s' '{{}}' ;;
esac"#,
            log.display()
        ),
    );
    let observation = observation(pr_row(&serde_json::json!({})), true);
    let (status, unhealthy) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(!unhealthy, "a guard refusal must not fail the pass: {status:?}");
    assert_eq!(status.results[0].outcome, "skipped");
    assert!(status.results[0].error.is_none(), "{status:?}");
    // Exactly one attempt: no retry, and no override.
    let calls = std::fs::read_to_string(&log).expect("log");
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("enablePullRequestAutoMerge"))
            .count(),
        1,
        "{calls}"
    );
    assert!(!calls.contains("GHAPP_ALLOW_QUEUE_REARM"), "{calls}");
}
