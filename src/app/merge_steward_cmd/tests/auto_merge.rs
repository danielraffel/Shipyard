//! Exact-head native auto-merge on a base with no merge queue but
//! GitHub-enforced required checks that include the approval context.
use super::*;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MOVED: &str = "cccccccccccccccccccccccccccccccccccccccc";

const POLICY_OK: &str = r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["shipyard-review[bot]"]'"#;

fn required(contexts: &[&str]) -> Vec<RequiredCheck> {
    contexts
        .iter()
        .map(|context| RequiredCheck {
            context: (*context).to_owned(),
            app_id: None,
        })
        .collect()
}

fn no_queue_policy(contexts: &[&str]) -> StewardPolicy {
    StewardPolicy {
        merge_queue: false,
        native_auto_merge: true,
        required_checks: required(contexts),
        ..queue_policy()
    }
}

/// `macos` green, `review-verdict` still running, GitHub reports `BLOCKED`.
fn pending_verdict_pr_json(head: &str) -> String {
    format!(
        r#"{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{head}","headRefName":"feature","mergeStateStatus":"BLOCKED","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-10-30T00:00:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}},{{"__typename":"CheckRun","name":"review-verdict","status":"IN_PROGRESS","conclusion":null,"detailsUrl":"https://github.com/owner/repo/actions/runs/101"}}]}}"#
    )
}

/// Every required check green and GitHub reports `CLEAN`: GitHub would merge
/// it now, so it cannot be armed.
fn clean_pr_json(head: &str) -> String {
    pending_verdict_pr_json(head)
        .replace(
            r#""mergeStateStatus":"BLOCKED""#,
            r#""mergeStateStatus":"CLEAN""#,
        )
        .replace(
            r#""status":"IN_PROGRESS","conclusion":null"#,
            r#""status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-10-30T00:01:00Z""#,
        )
}

/// [`script`] serving a `CLEAN`, all-green live PR.
fn clean_script(log: &Path, comments: &str) -> String {
    script(log, comments, HEAD, "null", "")
        .replace(&pending_verdict_pr_json(HEAD), &clean_pr_json(HEAD))
}

fn clean_pr() -> ObservedPr {
    parse_pr(
        &serde_json::from_str(&clean_pr_json(HEAD)).expect("PR json"),
        &BTreeMap::new(),
    )
    .expect("PR")
}

fn pending_verdict_pr() -> ObservedPr {
    parse_pr(
        &serde_json::from_str(&pending_verdict_pr_json(HEAD)).expect("PR json"),
        &BTreeMap::new(),
    )
    .expect("PR")
}

fn no_queue_observation(pr: ObservedPr) -> RepoObservation {
    let mut observation = observation_for(pr, false);
    observation.allow_auto_merge = true;
    observation.required_checks = required(&["macos", "review-verdict"]);
    observation
}

fn comment(body: &str) -> String {
    format!(
        r#"{{"id":12345,"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-30T12:00:00Z","body":"{body}","performed_via_github_app_id":99}}"#
    )
}

/// One fake `gh` for every case. `live_head` is the head GitHub reports now;
/// `armed_timeline` is the queue-state `autoMergeRequest` + timeline nodes.
fn script(log: &Path, comments: &str, live_head: &str, auto_merge: &str, nodes: &str) -> String {
    format!(
        r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/.shipyard/config.toml"*) {POLICY_OK} ;;
  *"enablePullRequestAutoMerge"*) printf '%s' '{{"data":{{"enablePullRequestAutoMerge":{{"pullRequest":{{"number":42}}}}}}}}' ;;
  *"mergePullRequest("*) printf '%s' '{{"data":{{"mergePullRequest":{{"pullRequest":{{"number":42,"merged":true}}}}}}}}' ;;
  *"disablePullRequestAutoMerge"*) printf '%s' '{{"data":{{"disablePullRequestAutoMerge":{{"pullRequest":{{"number":42}}}}}}}}' ;;
  *"enqueuePullRequest"*) echo "a no-queue base must never enqueue" >&2; exit 91 ;;
  *"pulls/42/reviews"*) printf '' ;;
  *"issues/42/comments"*) printf '%s\n' '{comments}' ;;
  *"commits/{HEAD}"*) printf '%s' '2026-10-30T00:00:00Z' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{live_head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":{auto_merge},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[{nodes}]}}}}}}}}}}' ;;
  *"query=query("*"mergeQueue"*) printf '%s' '{{"data":{{"repository":{{"mergeQueue":null}}}}}}' ;;
  "pr view "*) printf '%s' '{live_pr}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        log = log.display(),
        live_pr = pending_verdict_pr_json(live_head),
    )
}

fn run(
    temp: &tempfile::TempDir,
    body: &str,
) -> (Option<String>, Option<String>, String, StewardLedger) {
    run_with(temp, body, &pending_verdict_pr())
}

fn run_with(
    temp: &tempfile::TempDir,
    body: &str,
    pr: &ObservedPr,
) -> (Option<String>, Option<String>, String, StewardLedger) {
    let log = temp.path().join("calls");
    let actions = fake_gh(temp, body);
    let observation = no_queue_observation(pr.clone());
    let policy = no_queue_policy(&["macos", "review-verdict"]);
    let mut ledger = StewardLedger::default();
    let control = mutation_control(temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    assert_eq!(decision, StewardDecision::ArmMergeQueue);
    let (mutation, error) = mutate_pr(&context, pr, &policy, &decision, &mut ledger);
    let calls = fs::read_to_string(log).unwrap_or_default();
    (mutation, error, calls, ledger)
}

fn approved() -> String {
    comment(&format!("LGTM\\nreviewed: {HEAD}"))
}

#[test]
fn merge_path_requires_no_queue_auto_merge_and_a_required_approval_context() {
    assert_eq!(
        no_queue_policy(&["macos", "review-verdict"]).merge_path(),
        "native_auto_merge_exact_head"
    );
    for refused in [
        no_queue_policy(&[]),
        no_queue_policy(&["macos"]),
        StewardPolicy {
            native_auto_merge: false,
            ..no_queue_policy(&["macos", "review-verdict"])
        },
        StewardPolicy {
            auto_merge_approval_context: String::new(),
            ..no_queue_policy(&["macos", "review-verdict"])
        },
    ] {
        assert_eq!(
            refused.merge_path(),
            "direct_merge_refused_server_enforcement_required"
        );
        assert!(matches!(
            classify_pr(&pending_verdict_pr().fact, &refused, &BTreeMap::new()),
            StewardDecision::DirectMergeRefused { .. } | StewardDecision::WaitingRequired { .. }
        ));
        assert_ne!(
            classify_pr(&pending_verdict_pr().fact, &refused, &BTreeMap::new()),
            StewardDecision::ArmMergeQueue
        );
    }
    let custom = StewardPolicy {
        auto_merge_approval_context: "approval".to_owned(),
        ..no_queue_policy(&["macos", "approval"])
    };
    assert_eq!(custom.merge_path(), "native_auto_merge_exact_head");
}

#[test]
fn org_queue_path_is_unchanged() {
    let mut policy = queue_policy();
    policy.required_checks = required(&["macos", "review-verdict"]);
    assert_eq!(policy.merge_path(), "native_queue_exact_head");
    // A pending required check still waits on a queue base.
    assert_eq!(
        classify_pr(&pending_verdict_pr().fact, &policy, &BTreeMap::new()),
        StewardDecision::WaitingRequired {
            contexts: vec!["review-verdict".to_owned()]
        }
    );
}

#[test]
fn a_failed_required_check_is_never_armed() {
    let mut pr = pending_verdict_pr();
    for check in &mut pr.fact.checks {
        check.status = "COMPLETED".to_owned();
        check.conclusion = Some("FAILURE".to_owned());
        check.run_id = None;
    }
    assert!(matches!(
        classify_pr(
            &pr.fact,
            &no_queue_policy(&["macos", "review-verdict"]),
            &BTreeMap::new()
        ),
        StewardDecision::RequiredFailed { .. }
    ));
}

#[cfg(unix)]
#[test]
fn approved_head_arms_auto_merge_with_expected_head_oid() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(&log, &approved(), HEAD, "null", "");
    let (mutation, error, calls, ledger) = run(&temp, &body);
    assert!(error.is_none(), "{error:?}\n{calls}");
    assert_eq!(mutation, Some(format!("auto_merge_enabled:{HEAD}")));
    let arm = calls
        .lines()
        .find(|line| line.contains("enablePullRequestAutoMerge"))
        .expect("arm call");
    assert!(arm.contains("expectedHeadOid:$head"), "{arm}");
    assert!(arm.contains("mergeMethod:MERGE"), "{arm}");
    assert!(arm.contains(&format!("head={HEAD}")), "{arm}");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
    assert!(
        ledger
            .audit
            .iter()
            .any(|entry| entry.action == "enable_auto_merge_exact_head"
                && entry.subject == format!("pr:42:{HEAD}"))
    );
    let audit = fs::read_to_string(temp.path().join("state/merge_queue/mutations.jsonl"))
        .expect("mutation audit");
    assert!(audit.contains("auto_merge_enabled"), "{audit}");
    assert!(audit.contains("\"approval_comment_id\":12345"), "{audit}");
}

#[cfg(unix)]
#[test]
fn unapproved_or_held_head_is_never_armed() {
    let hold = format!(
        "{}\n{}",
        approved(),
        r#"{"id":12346,"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-30T13:00:00Z","body":"verdict: HOLD head:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
    );
    let earlier = comment(&format!("LGTM\\nreviewed: {MOVED}"));
    for comments in [String::new(), hold, earlier] {
        for clean in [false, true] {
            let temp = tempfile::tempdir().expect("temp");
            let log = temp.path().join("calls");
            let (body, pr) = if clean {
                (clean_script(&log, &comments), clean_pr())
            } else {
                (
                    script(&log, &comments, HEAD, "null", ""),
                    pending_verdict_pr(),
                )
            };
            let (mutation, error, calls, _) = run_with(&temp, &body, &pr);
            assert_eq!(
                mutation.as_deref(),
                Some("waiting_head_approval"),
                "{comments} clean={clean}: {error:?}\n{calls}"
            );
            assert!(!calls.contains("AutoMerge("), "{calls}");
            assert!(!calls.contains("mergePullRequest("), "{calls}");
        }
    }
}

#[cfg(unix)]
#[test]
fn moved_head_is_never_armed() {
    // GitHub's live PR moved: the gate is re-evaluated on the new head, which
    // carries no approval.
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(&log, &approved(), MOVED, "null", "");
    let (mutation, error, calls, _) = run(&temp, &body);
    assert_eq!(
        mutation.as_deref(),
        Some("waiting_head_approval"),
        "{error:?}\n{calls}"
    );
    assert!(error.is_some_and(|error| error.contains("cccccccccccc")));
    assert!(!calls.contains("AutoMerge("), "{calls}");

    // The head moved between the PR read and the gate's queue-state read.
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(&log, &approved(), HEAD, "null", "").replacen(
        &format!(r#""headRefOid":"{HEAD}","isInMergeQueue""#),
        &format!(r#""headRefOid":"{MOVED}","isInMergeQueue""#),
        1,
    );
    let (mutation, error, calls, _) = run(&temp, &body);
    assert_eq!(
        mutation.as_deref(),
        Some("skipped_after_live_revalidation"),
        "{error:?}\n{calls}"
    );
    assert!(!calls.contains("AutoMerge("), "{calls}");
}

#[cfg(unix)]
#[test]
fn an_arm_for_this_head_is_left_alone_and_one_for_an_earlier_head_is_disarmed() {
    let arrival = format!(
        r#"{{"__typename":"PullRequestCommit","commit":{{"oid":"{HEAD}","checkSuites":{{"nodes":[{{"createdAt":"2026-10-30T01:00:00Z"}}]}}}}}}"#
    );
    let armed = r#"{"enabledAt":"2026-10-30T02:00:00Z"}"#;

    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let after = format!(
        r#"{arrival},{{"__typename":"AutoMergeEnabledEvent","createdAt":"2026-10-30T02:00:00Z","actor":{{"login":"x"}}}}"#
    );
    let body = script(&log, &approved(), HEAD, armed, &after);
    let (mutation, error, calls, _) = run(&temp, &body);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(mutation, Some(format!("auto_merge_already_enabled:{HEAD}")));
    assert!(!calls.contains("AutoMerge("), "{calls}");

    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let before = format!(
        r#"{{"__typename":"AutoMergeEnabledEvent","createdAt":"2026-10-30T00:30:00Z","actor":{{"login":"x"}}}},{arrival}"#
    );
    let body = script(&log, &approved(), HEAD, armed, &before);
    let (mutation, error, calls, _) = run(&temp, &body);
    assert!(error.is_none(), "{error:?}\n{calls}");
    assert_eq!(
        mutation,
        Some(format!("auto_merge_disabled_head_moved:{HEAD}"))
    );
    assert!(calls.contains("disablePullRequestAutoMerge"), "{calls}");
    assert!(!calls.contains("enablePullRequestAutoMerge"), "{calls}");
}

#[cfg(unix)]
#[test]
fn clean_approved_head_is_merged_with_expected_head_oid() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = clean_script(&log, &approved());
    let (mutation, error, calls, ledger) = run_with(&temp, &body, &clean_pr());
    assert!(error.is_none(), "{error:?}\n{calls}");
    assert_eq!(mutation, Some(format!("merged_exact_head:{HEAD}")));
    let merge = calls
        .lines()
        .find(|line| line.contains("mergePullRequest("))
        .expect("merge call");
    assert!(merge.contains("expectedHeadOid:$head"), "{merge}");
    assert!(merge.contains("mergeMethod:MERGE"), "{merge}");
    assert!(merge.contains(&format!("head={HEAD}")), "{merge}");
    assert!(!calls.contains("AutoMerge("), "{calls}");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
    assert!(
        ledger
            .audit
            .iter()
            .any(|entry| entry.action == "merge_exact_head_clean"
                && entry.subject == format!("pr:42:{HEAD}"))
    );
    let audit = fs::read_to_string(temp.path().join("state/merge_queue/mutations.jsonl"))
        .expect("mutation audit");
    assert!(audit.contains("merged_exact_head"), "{audit}");
}

#[cfg(unix)]
#[test]
fn a_base_whose_ruleset_lacks_the_approval_context_neither_arms_nor_merges() {
    for (pr, json) in [
        (clean_pr(), clean_pr_json(HEAD)),
        (pending_verdict_pr(), pending_verdict_pr_json(HEAD)),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let body = script(&log, &approved(), HEAD, "null", "")
            .replace(&pending_verdict_pr_json(HEAD), &json);
        let actions = fake_gh(&temp, &body);
        let mut observation = no_queue_observation(pr.clone());
        observation.required_checks = required(&["macos"]);
        let policy = no_queue_policy(&["macos"]);
        let mut ledger = StewardLedger::default();
        let control = mutation_control(&temp, "studio", "studio");
        let ledger_path = temp.path().join("ledger.json");
        let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
        let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
        assert_ne!(decision, StewardDecision::ArmMergeQueue);
        let (mutation, error) = mutate_pr(&context, &pr, &policy, &decision, &mut ledger);
        assert!(
            mutation.is_none() && error.is_none(),
            "{mutation:?} {error:?}"
        );
        let calls = fs::read_to_string(log).unwrap_or_default();
        assert!(!calls.contains("AutoMerge("), "{calls}");
        assert!(!calls.contains("mergePullRequest("), "{calls}");
    }
}
