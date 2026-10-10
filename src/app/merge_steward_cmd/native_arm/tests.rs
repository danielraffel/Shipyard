// Everything that drives a fake `gh` is unix-only, because the stand-in is a
// shell script. Under `-D warnings` an ungated helper that only unix tests call
// is a dead-code error on Windows, so the gating has to match the tests exactly.
#[cfg(unix)]
use std::collections::BTreeMap;

#[cfg(unix)]
use super::apply_native_arm_backstop;
use super::steward_declines_ownership;
#[cfg(unix)]
use crate::app::merge_steward_cmd::{ObservedPr, PrReport, RepoObservation, parse_pr};
#[cfg(unix)]
use crate::cloud::GitHubActions;
use crate::merge_steward::StewardDecision;
#[cfg(unix)]
use crate::merge_steward::{CapacityPreemptionPolicy, RequiredCheck};

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
  *contents/.shipyard/config.toml*)
    printf '%s\n' 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
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
  *contents/.shipyard/config.toml*)
    printf '%s\n' 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
  *) printf '%s' '{{}}' ;;
esac"#
    )
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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
    let observation = observation(
        pr_row(&serde_json::json!({"mergeStateStatus": "BLOCKED"})),
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
  *contents/.shipyard/config.toml*)
    printf '%s\n' 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
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
  *contents/.shipyard/config.toml*)
    printf '%s\n' 'gh: Not Found (HTTP 404)' >&2; exit 1 ;;
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
    assert!(
        !unhealthy,
        "a guard refusal must not fail the pass: {status:?}"
    );
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

// ---------------------------------------------------------------------------
// Head approval
// ---------------------------------------------------------------------------

/// Script for a never-armed PR on a repository whose base requires head
/// approval, with `reviews` as the flattened review records `gh` would print.
#[cfg(unix)]
fn approval_required(log: &str, reviews: &str) -> String {
    format!(
        r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *enablePullRequestAutoMerge*)
    printf '%s' '{{"data":{{"enablePullRequestAutoMerge":{{"pullRequest":{{"number":42}}}}}}}}' ;;
  *isInMergeQueue*)
    printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *contents/.shipyard/config.toml*)
    printf '[auto_merge]\narm_requires_head_approval = true\n' ;;
  *pulls/42/reviews*)
    printf '%s' '{reviews}' ;;
  *issues/42/comments*)
    printf '' ;;
  *) printf '%s' '{{}}' ;;
esac"#
    )
}

/// The backstop arms green, unarmed pull requests nobody handed to the
/// steward, so it is exactly where a rebased, unreviewed head would be
/// re-armed. On a repository that arms only approved heads it must not.
#[cfg(unix)]
#[test]
fn the_backstop_leaves_an_unapproved_head_disarmed() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let actions = fake_gh(&temp, &approval_required(&log.display().to_string(), ""));
    let observation = observation(pr_row(&serde_json::json!({})), true);
    let (status, unhealthy) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(!unhealthy, "{status:?}");
    assert_eq!(status.results[0].outcome, "skipped");
    assert!(
        matches!(
            status.results[0].skip,
            Some(crate::auto_arm::ArmSkip::HeadNotApproved { .. })
        ),
        "{status:?}"
    );
    let calls = std::fs::read_to_string(&log).expect("log");
    assert!(
        calls.contains("contents/.shipyard/config.toml?ref=main"),
        "{calls}"
    );
    assert!(!calls.contains("enablePullRequestAutoMerge"), "{calls}");
}

/// Control on the same instrument: an approving review of this head arms it.
#[cfg(unix)]
#[test]
fn the_backstop_arms_a_head_with_an_approving_review() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls.log");
    let review = r#"{"login":"someone","type":"User","at":"2026-10-04T01:00:00Z","state":"APPROVED","commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
    let actions = fake_gh(
        &temp,
        &approval_required(&log.display().to_string(), review),
    );
    let observation = observation(pr_row(&serde_json::json!({})), true);
    let (status, unhealthy) = apply_native_arm_backstop(
        &actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(!unhealthy, "{status:?}");
    assert_eq!(status.results[0].outcome, "armed", "{status:?}");
    let calls = std::fs::read_to_string(&log).expect("log");
    assert_eq!(
        calls.matches("enablePullRequestAutoMerge").count(),
        1,
        "{calls}"
    );
}

/// A fake `gh` for #8678's real `failed_checks` ejection, whose merge group's
/// required `macos` check concluded `cancelled` on Actions job 7 described by
/// `job`. The repository opts in to same-head re-enqueues on its base.
#[cfg(unix)]
fn ejected_with_cancelled_macos(
    temp: &tempfile::TempDir,
    job: &serde_json::Value,
) -> GitHubActions {
    ejected_with_timeline(temp, job, &timeline_8678(Some("2026-09-22T23:20:00Z"), &[]))
}

/// #8678's real queue-state response, with the current head's first check
/// suite at `arrived` (the capture carries none) and `extra` timeline nodes
/// appended.
#[cfg(unix)]
fn timeline_8678(arrived: Option<&str>, extra: &[serde_json::Value]) -> serde_json::Value {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/github/pr_real_8678_first_environment_ejection.json"
    ))
    .expect("fixture");
    let mut value: serde_json::Value = serde_json::from_str(&raw).expect("fixture JSON");
    let nodes = value
        .pointer_mut("/data/repository/pullRequest/timelineItems/nodes")
        .and_then(serde_json::Value::as_array_mut)
        .expect("nodes");
    for node in nodes.iter_mut() {
        if let Some(at) = arrived
            && node
                .pointer("/commit/oid")
                .and_then(serde_json::Value::as_str)
                == Some("f0fb2fb38ef5c900efad7ea0630906f081db53a9")
        {
            node["commit"]["checkSuites"] = serde_json::json!({"nodes": [{"createdAt": at}]});
        }
    }
    nodes.extend(extra.iter().cloned());
    value
}

#[cfg(unix)]
fn ejected_with_timeline(
    temp: &tempfile::TempDir,
    job: &serde_json::Value,
    timeline: &serde_json::Value,
) -> GitHubActions {
    let dir = temp.path();
    let fixture_path = dir.join("queue_state.json");
    std::fs::write(&fixture_path, timeline.to_string()).expect("timeline");
    let fixture = fixture_path.display().to_string();
    let group = "2410ca497342cfc0264bf5b72713de0b56099a0f";
    let files = [
        ("config", "[queue.environment_requeue]\nenabled = true\n".to_owned()),
        ("rules", "[]".to_owned()),
        ("classic", r#"{"contexts":["macos"]}"#.to_owned()),
        ("pull", r#"{"base":{"ref":"main"}}"#.to_owned()),
        (
            "runs",
            r#"{"total_count":1,"check_runs":[{"id":7,"name":"macos","status":"completed","conclusion":"cancelled","app":{"slug":"github-actions"}}]}"#
                .to_owned(),
        ),
        ("status", r#"{"statuses":[]}"#.to_owned()),
        ("job", job.to_string()),
        (
            "armed",
            r#"{"data":{"enablePullRequestAutoMerge":{"pullRequest":{"number":8678}}}}"#.to_owned(),
        ),
    ];
    for (name, body) in &files {
        std::fs::write(dir.join(name), body).expect("answer");
    }
    let log = dir.join("calls.log");
    let d = dir.display();
    fake_gh(
        temp,
        &format!(
            r#"printf '%s\n' "$*" >> '{log}'
case "$*" in
  *enablePullRequestAutoMerge*) cat '{d}/armed' ;;
  *isInMergeQueue*) cat '{fixture}' ;;
  *contents/.shipyard/config.toml*) cat '{d}/config' ;;
  *rules/branches/main*) cat '{d}/rules' ;;
  *protection/required_status_checks*) cat '{d}/classic' ;;
  *pulls/8678*) cat '{d}/pull' ;;
  *commits/{group}/check-runs*) cat '{d}/runs' ;;
  *commits/{group}/status*) cat '{d}/status' ;;
  *actions/jobs/7*) cat '{d}/job' ;;
  *) printf '%s' '{{}}' ;;
esac"#,
            log = log.display()
        ),
    )
}

#[cfg(unix)]
fn run_backstop_on_8678(actions: &GitHubActions) -> super::NativeArmResult {
    let head = "f0fb2fb38ef5c900efad7ea0630906f081db53a9";
    let observation = observation(pr_row(&serde_json::json!({"headRefOid": head})), true);
    let (status, unhealthy) = apply_native_arm_backstop(
        actions,
        &observation,
        &report(StewardDecision::Unmanaged),
        "shipyard:no-auto-merge",
        true,
    );
    assert!(!unhealthy, "{status:?}");
    status.results[0].clone()
}

/// The unattended path for #9650/#9657/#9658: a required job cancelled after
/// 16 minutes with no runner ejected the head. The backstop re-arms that exact
/// head, bound to it with `expectedHeadOid`.
#[cfg(unix)]
#[test]
fn the_backstop_rearms_a_head_ejected_by_a_starved_required_job_at_that_head() {
    let temp = tempfile::tempdir().expect("temp");
    let job = serde_json::json!({"id": 7, "status": "completed", "conclusion": "cancelled",
        "created_at": "2026-09-23T03:30:00Z", "completed_at": "2026-09-23T03:46:00Z",
        "runner_name": "", "steps": []});
    let actions = ejected_with_cancelled_macos(&temp, &job);
    let result = run_backstop_on_8678(&actions);
    assert_eq!(result.outcome, "rearmed_same_head", "{result:?}");
    let calls = std::fs::read_to_string(temp.path().join("calls.log")).expect("log");
    let arm = calls
        .lines()
        .find(|call| call.contains("enablePullRequestAutoMerge"))
        .expect("an arm mutation");
    assert!(arm.contains("expectedHeadOid:$head"), "{arm}");
    assert!(
        arm.contains("head=f0fb2fb38ef5c900efad7ea0630906f081db53a9"),
        "{arm}"
    );
}

/// Negative control: the same ejection, but a runner took the job before it
/// was cancelled. That is not starvation, so the head stays disarmed.
#[cfg(unix)]
#[test]
fn the_backstop_leaves_a_head_disarmed_when_the_cancelled_job_had_a_runner() {
    let temp = tempfile::tempdir().expect("temp");
    let job = serde_json::json!({"id": 7, "status": "completed", "conclusion": "cancelled",
        "created_at": "2026-09-23T03:30:00Z", "completed_at": "2026-09-23T03:46:00Z",
        "runner_name": "studio-pulp-gate-01", "steps": []});
    let actions = ejected_with_cancelled_macos(&temp, &job);
    let result = run_backstop_on_8678(&actions);
    assert_eq!(result.outcome, "skipped", "{result:?}");
    let calls = std::fs::read_to_string(temp.path().join("calls.log")).expect("log");
    assert!(
        calls.contains("actions/jobs/7"),
        "the job was read: {calls}"
    );
    assert!(!calls.contains("enablePullRequestAutoMerge"), "{calls}");
}

#[cfg(unix)]
fn starved_job() -> serde_json::Value {
    serde_json::json!({"id": 7, "status": "completed", "conclusion": "cancelled",
        "created_at": "2026-09-23T03:30:00Z", "completed_at": "2026-09-23T03:46:00Z",
        "runner_name": "", "steps": []})
}

/// The same starved ejection is re-armed only when someone armed the current
/// head after it arrived. Each control removes that one fact.
#[cfg(unix)]
#[test]
fn the_backstop_rearms_only_a_head_someone_armed_after_it_arrived() {
    let without_arms = {
        let mut value = timeline_8678(Some("2026-09-22T23:20:00Z"), &[]);
        value["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]
            .as_array_mut()
            .expect("nodes")
            .retain(|node| node["__typename"] != "AutoMergeEnabledEvent");
        value
    };
    let mut truncated = timeline_8678(None, &[]);
    truncated["data"]["repository"]["pullRequest"]["timelineItems"]["pageInfo"]["hasPreviousPage"] =
        serde_json::json!(true);
    let controls = [
        (
            "the arms were for earlier heads; the current head was force-pushed after them",
            timeline_8678(
                None,
                &[serde_json::json!({"__typename": "HeadRefForcePushedEvent",
                    "createdAt": "2026-09-23T00:00:00Z",
                    "afterCommit": {"oid": "f0fb2fb38ef5c900efad7ea0630906f081db53a9"}})],
            ),
        ),
        ("no arm event at all", without_arms),
        (
            "the head's first check suite came after the last arm",
            timeline_8678(Some("2026-09-23T00:00:00Z"), &[]),
        ),
        (
            "a truncated window: refused by the classifier before the arrival check",
            truncated,
        ),
    ];
    for (why, timeline) in controls {
        let temp = tempfile::tempdir().expect("temp");
        let actions = ejected_with_timeline(&temp, &starved_job(), &timeline);
        let result = run_backstop_on_8678(&actions);
        assert_eq!(result.outcome, "skipped", "{why}: {result:?}");
        let calls = std::fs::read_to_string(temp.path().join("calls.log")).expect("log");
        assert!(
            !calls.contains("enablePullRequestAutoMerge"),
            "{why}: {calls}"
        );
    }
    // Positive control on the same instrument: arrival at 23:20, armed 23:42.
    let temp = tempfile::tempdir().expect("temp");
    let actions = ejected_with_timeline(
        &temp,
        &starved_job(),
        &timeline_8678(Some("2026-09-22T23:20:00Z"), &[]),
    );
    assert_eq!(run_backstop_on_8678(&actions).outcome, "rearmed_same_head");
}

/// The arrival check fails closed on its own. Through the backstop a
/// truncated window is refused earlier, by the classifier's complete-timeline
/// rule, so this branch is exercised directly.
#[cfg(unix)]
#[test]
fn an_arrival_the_window_cannot_place_refuses_the_rearm() {
    let allowed = crate::environment_requeue::EnvironmentRequeue {
        allowed: true,
        class: Some(crate::environment_requeue::RequeueClass::Interruption),
        reason: "starved".to_owned(),
        merge_group_commit: None,
        evidence: Vec::new(),
    };
    let head = "f0fb2fb38ef5c900efad7ea0630906f081db53a9";
    let mut truncated = timeline_8678(None, &[]);
    truncated["data"]["repository"]["pullRequest"]["timelineItems"]["pageInfo"]["hasPreviousPage"] =
        serde_json::json!(true);
    let refused = super::armed_since_arrival(allowed.clone(), &truncated, head);
    assert!(!refused.allowed);
    assert!(
        refused.reason.contains("cannot be read"),
        "{}",
        refused.reason
    );
    // Control: the same verdict with the arrival placed before the last arm.
    let placed = timeline_8678(Some("2026-09-22T23:20:00Z"), &[]);
    assert!(super::armed_since_arrival(allowed, &placed, head).allowed);
}
