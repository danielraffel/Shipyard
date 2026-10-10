use std::cell::RefCell;

use super::{ArmOutcome, arm_for_request, arm_native_auto_merge};

/// A scripted `gh` that records every argv it was handed.
struct FakeGh {
    /// Responses matched by a substring of the joined argv, in order of check.
    routes: Vec<(&'static str, Result<String, String>)>,
    calls: RefCell<Vec<String>>,
}

impl FakeGh {
    /// A repository with no `.shipyard/config.toml` on its base answers 404,
    /// so head approval is not required unless a test scripts the file.
    fn new(mut routes: Vec<(&'static str, Result<String, String>)>) -> Self {
        routes.push((
            "contents/.shipyard/config.toml",
            Err("gh: Not Found (HTTP 404)".to_owned()),
        ));
        Self {
            routes,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn run(&self, args: &[String]) -> Result<String, String> {
        let joined = args.join(" ");
        self.calls.borrow_mut().push(joined.clone());
        for (needle, response) in &self.routes {
            if joined.contains(needle) {
                return response.clone();
            }
        }
        Err(format!("unscripted gh call: {joined}"))
    }

    fn called(&self, needle: &str) -> bool {
        self.calls.borrow().iter().any(|call| call.contains(needle))
    }

    fn call_count(&self) -> usize {
        self.calls.borrow().len()
    }
}

fn pr_view(node_id: &str, draft: bool) -> String {
    format!(r#"{{"id":"{node_id}","isDraft":{draft},"baseRefName":"main"}}"#)
}

/// A `PR_QUEUE_STATE_QUERY` response for an open, never-armed pull request.
fn never_armed_state() -> String {
    r#"{"data":{"repository":{"pullRequest":{
        "number":7,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,
        "timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[]}}}}}"#
        .to_owned()
}

fn queued_state() -> String {
    r#"{"data":{"repository":{"pullRequest":{
        "number":7,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "isInMergeQueue":true,"mergeQueueEntry":{"state":"AWAITING_CHECKS","position":2},
        "autoMergeRequest":null,
        "timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[]}}}}}"#
        .to_owned()
}

fn armed_state() -> String {
    r#"{"data":{"repository":{"pullRequest":{
        "number":7,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "isInMergeQueue":false,"mergeQueueEntry":null,
        "autoMergeRequest":{"enabledAt":"2026-09-25T00:00:00Z"},
        "timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[]}}}}}"#
        .to_owned()
}

fn arm_accepted() -> String {
    r#"{"data":{"enablePullRequestAutoMerge":{"pullRequest":{"number":7}}}}"#.to_owned()
}

fn run(gh: &FakeGh) -> ArmOutcome {
    arm_native_auto_merge(&|args| gh.run(args), "owner/repo", 7, false, None)
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[test]
fn a_never_armed_pr_is_armed_with_merge_and_reported() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(never_armed_state())),
        ("enablePullRequestAutoMerge", Ok(arm_accepted())),
    ]);
    let outcome = run(&gh);
    assert!(outcome.armed, "{}", outcome.line);
    assert!(outcome.line.contains("Auto-merge armed on #7"));
    assert!(gh.called("mergeMethod:MERGE"));
    assert!(gh.called("id=PR_node"));
}

/// A squash subject folds the version-bump marker commit in and trips release
/// automation. No arm request may ever name a squash.
#[test]
fn the_arm_request_never_names_squash() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(never_armed_state())),
        ("enablePullRequestAutoMerge", Ok(arm_accepted())),
    ]);
    assert!(run(&gh).armed);
    assert!(!gh.called("SQUASH"));
    assert!(!gh.called("squash"));
}

/// The internal marker makes the `ghapp` arm guard step aside. This request is
/// not bound to a validated head, so it must be judged by the guard.
#[test]
fn the_arm_request_does_not_claim_shipyards_internal_queue_marker() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(never_armed_state())),
        ("enablePullRequestAutoMerge", Ok(arm_accepted())),
    ]);
    assert!(run(&gh).armed);
    assert!(!gh.called("SHIPYARD_INTERNAL_QUEUE_MUTATION"));
}

// ---------------------------------------------------------------------------
// Idempotency and guard agreement
// ---------------------------------------------------------------------------

/// The `auto_merge == null` trap: a queued PR reads as unarmed to a naive
/// reader, and re-arming re-enqueues a head the queue already holds.
#[test]
fn a_queued_pr_is_not_re_armed_and_no_mutation_is_issued() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(queued_state())),
    ]);
    let outcome = run(&gh);
    assert!(!outcome.armed);
    assert!(outcome.line.contains("already in the merge queue"));
    assert!(
        !gh.called("enablePullRequestAutoMerge"),
        "issued a mutation for a queued PR"
    );
}

#[test]
fn an_already_armed_pr_issues_no_mutation_so_reruns_are_idempotent() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(armed_state())),
    ]);
    let outcome = run(&gh);
    assert!(!outcome.armed);
    assert!(outcome.line.contains("already armed"));
    assert!(!gh.called("enablePullRequestAutoMerge"));
}

#[test]
fn a_draft_is_never_armed() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", true))),
        ("isInMergeQueue", Ok(never_armed_state())),
    ]);
    let outcome = run(&gh);
    assert!(!outcome.armed);
    assert!(outcome.line.contains("draft"));
    assert!(!gh.called("enablePullRequestAutoMerge"));
}

/// A guard refusal is agreement, not a fault: it must read as a normal outcome
/// and must not be retried or overridden.
#[test]
fn a_guard_refusal_is_reported_as_agreement_and_never_overridden() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(never_armed_state())),
        (
            "enablePullRequestAutoMerge",
            Err("queue-arm-guard: refusing: PR #7 was ejected for failed_checks".to_owned()),
        ),
    ]);
    let outcome = run(&gh);
    assert!(!outcome.armed);
    assert!(outcome.line.contains("queue-arm guard declined it"));
    assert!(outcome.line.starts_with('▸'), "{}", outcome.line);
    assert!(!outcome.line.contains("GHAPP_ALLOW_QUEUE_REARM"));
    // Exactly one mutation attempt: no retry.
    assert_eq!(
        gh.calls
            .borrow()
            .iter()
            .filter(|call| call.contains("enablePullRequestAutoMerge"))
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// Failing closed
// ---------------------------------------------------------------------------

/// An unreadable state is not an unarmed state.
#[test]
fn an_unreadable_queue_state_arms_nothing() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Err("HTTP 502: Bad Gateway".to_owned())),
    ]);
    let outcome = run(&gh);
    assert!(!outcome.armed);
    assert!(outcome.line.contains("refusing to arm blind"));
    assert!(!gh.called("enablePullRequestAutoMerge"));
}

#[test]
fn unreadable_pr_facts_arm_nothing_and_never_reach_the_state_read() {
    let gh = FakeGh::new(vec![("pr view", Err("HTTP 404".to_owned()))]);
    let outcome = run(&gh);
    assert!(!outcome.armed);
    assert!(outcome.line.contains("could not be read"));
    assert_eq!(gh.call_count(), 1);
}

/// GitHub answers a rejected mutation with HTTP 200 plus an `errors` array, so
/// a bare `Ok` must not be read as success.
#[test]
fn a_graphql_error_payload_is_not_read_as_armed() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(never_armed_state())),
        (
            "enablePullRequestAutoMerge",
            Ok(r#"{"data":{"enablePullRequestAutoMerge":null},
                 "errors":[{"message":"Pull request is in unstable status"}]}"#
                .to_owned()),
        ),
    ]);
    let outcome = run(&gh);
    assert!(!outcome.armed);
    assert!(outcome.line.contains("Pull request is in unstable status"));
}

#[test]
fn an_ordinary_arm_failure_says_the_ship_continues() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(never_armed_state())),
        (
            "enablePullRequestAutoMerge",
            Err("GraphQL: Resource not accessible by integration".to_owned()),
        ),
    ]);
    let outcome = run(&gh);
    assert!(!outcome.armed);
    assert!(outcome.line.contains("The ship continues"));
}

/// A multi-line `gh` diagnostic must not break the one-result-per-line
/// transcript the ship renders.
#[test]
fn a_multi_line_diagnostic_is_collapsed_to_one_line() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(never_armed_state())),
        (
            "enablePullRequestAutoMerge",
            Err("gh exited 1\nstderr: boom\n\nmore".to_owned()),
        ),
    ]);
    let outcome = run(&gh);
    assert!(!outcome.armed);
    assert!(!outcome.line.contains('\n'), "{}", outcome.line);
}

#[test]
fn a_repo_without_owner_and_name_arms_nothing() {
    let gh = FakeGh::new(vec![("pr view", Ok(pr_view("PR_node", false)))]);
    let outcome = arm_native_auto_merge(&|args| gh.run(args), "not-a-slug", 7, false, None);
    assert!(!outcome.armed);
    assert!(outcome.line.contains("could not be read"));
    assert!(!gh.called("enablePullRequestAutoMerge"));
}

// ---------------------------------------------------------------------------
// Where the result is written
// ---------------------------------------------------------------------------

/// `--json` puts exactly one envelope on stdout. A plain arm line there would
/// corrupt it for every consumer that parses this command's output.
#[test]
fn json_mode_keeps_the_arm_line_off_stdout() {
    let outcome = ArmOutcome {
        armed: true,
        line: "▸ Auto-merge armed on #7".to_owned(),
    };
    let mut stdout = Vec::new();
    super::super::report_arm_outcome(&outcome, true, &mut stdout).expect("reported");
    assert!(
        stdout.is_empty(),
        "stdout must stay envelope-only: {:?}",
        String::from_utf8_lossy(&stdout)
    );
}

#[test]
fn human_mode_writes_the_arm_line_to_stdout() {
    let outcome = ArmOutcome {
        armed: false,
        line: "▸ Auto-merge left as it is on #7: it is a draft".to_owned(),
    };
    let mut stdout = Vec::new();
    super::super::report_arm_outcome(&outcome, false, &mut stdout).expect("reported");
    let text = String::from_utf8(stdout).expect("utf8");
    assert!(text.contains("Auto-merge left as it is on #7"), "{text}");
    assert!(text.ends_with('\n'), "{text:?}");
}

/// A repository with auto-merge switched off must read as "nothing to do", not
/// as a warning on every ship.
#[test]
fn a_repository_without_auto_merge_reads_as_nothing_to_do() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(never_armed_state())),
        (
            "enablePullRequestAutoMerge",
            Err("GraphQL: Auto merge is not allowed for this repository".to_owned()),
        ),
    ]);
    let outcome = run(&gh);
    assert!(!outcome.armed);
    assert!(
        outcome.line.starts_with('▸'),
        "must not be a warning: {}",
        outcome.line
    );
    assert!(outcome.line.contains("does not allow native auto-merge"));
}

// ---------------------------------------------------------------------------
// Head approval
// ---------------------------------------------------------------------------

use crate::head_approval::tests::{PR_9438_HEADS, pr_9438_bot_comments, pr_9438_queue};

const REQUIRES_APPROVAL: &str =
    "[auto_merge]\narm_requires_head_approval = true\nreviewer_logins = [\"danielraffel\"]\n";

fn jsonl(records: &[serde_json::Value]) -> String {
    records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One arm attempt on #9438 after a rebase to `head`, as `shipyard pr` makes
/// it, on a repository that arms only approved heads. Returns the outcome and
/// how many arm mutations were sent.
fn ship_9438_head(
    head: &str,
    comments: &[serde_json::Value],
    override_by: Option<&str>,
) -> (ArmOutcome, usize) {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_9438", false))),
        ("isInMergeQueue", Ok(pr_9438_queue(head).to_string())),
        (
            "contents/.shipyard/config.toml",
            Ok(REQUIRES_APPROVAL.to_owned()),
        ),
        ("pulls/9438/reviews", Ok(String::new())),
        ("issues/9438/comments", Ok(jsonl(comments))),
        ("enablePullRequestAutoMerge", Ok(arm_accepted())),
    ]);
    let outcome = arm_native_auto_merge(
        &|args| gh.run(args),
        "Generous-Corp/pulp",
        9438,
        false,
        override_by,
    );
    let arms = gh
        .calls
        .borrow()
        .iter()
        .filter(|call| call.contains("enablePullRequestAutoMerge"))
        .count();
    (outcome, arms)
}

/// Negative control: #9438 was re-armed after every rebase. Replayed with the
/// policy on, no rebased head is armed, and each result says why.
#[test]
fn replaying_9438_leaves_the_pr_disarmed_after_every_rebase() {
    let comments = pr_9438_bot_comments();
    let mut arms = 0;
    for head in PR_9438_HEADS {
        let (outcome, sent) = ship_9438_head(head, &comments, None);
        arms += sent;
        assert!(!outcome.armed, "{head}: {}", outcome.line);
        assert!(
            outcome.line.contains("carries no approval") && outcome.line.contains(&head[..12]),
            "{}",
            outcome.line
        );
    }
    assert_eq!(arms, 0);
}

/// Positive control: the same sequence with a listed reviewer's marker for
/// the last rebased head arms exactly once, on that head.
#[test]
fn replaying_9438_with_a_marker_for_the_rebased_head_arms_exactly_once() {
    let mut comments = pr_9438_bot_comments();
    comments.push(serde_json::json!({"login": "danielraffel", "type": "User",
        "at": "2026-10-04T04:45:00Z", "body": format!("reviewed:{}", PR_9438_HEADS[6])}));
    let mut armed_heads = Vec::new();
    let mut arms = 0;
    for head in PR_9438_HEADS {
        let (outcome, sent) = ship_9438_head(head, &comments, None);
        arms += sent;
        if outcome.armed {
            armed_heads.push(*head);
        }
    }
    assert_eq!(arms, 1);
    assert_eq!(armed_heads, vec![PR_9438_HEADS[6]]);
}

/// `--arm` arms an unapproved head on purpose, and the result says who did.
#[test]
fn an_explicit_arm_overrides_the_policy_and_names_who_armed() {
    let (outcome, arms) = ship_9438_head(
        PR_9438_HEADS[0],
        &pr_9438_bot_comments(),
        Some("operator@studio"),
    );
    assert!(outcome.armed, "{}", outcome.line);
    assert_eq!(arms, 1);
    assert!(
        outcome
            .line
            .contains("explicit `--arm` from operator@studio"),
        "{}",
        outcome.line
    );
}

/// An unreadable policy is not an absent one.
#[test]
fn an_unreadable_approval_policy_arms_nothing() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(never_armed_state())),
        (
            "contents/.shipyard/config.toml",
            Err("HTTP 502: Bad Gateway".to_owned()),
        ),
        ("enablePullRequestAutoMerge", Ok(arm_accepted())),
    ]);
    let outcome = run(&gh);
    assert!(!outcome.armed, "{}", outcome.line);
    assert!(
        outcome.line.contains("refusing to arm blind"),
        "{}",
        outcome.line
    );
    assert!(!gh.called("enablePullRequestAutoMerge"));
}

/// The policy is read from the pull request's base branch, which a head
/// cannot change.
#[test]
fn the_approval_policy_is_read_from_the_base_branch() {
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node", false))),
        ("isInMergeQueue", Ok(never_armed_state())),
        ("enablePullRequestAutoMerge", Ok(arm_accepted())),
    ]);
    assert!(run(&gh).armed);
    assert!(gh.called("repos/owner/repo/contents/.shipyard/config.toml?ref=main"));
    assert!(gh.called("baseRefName"));
}

/// What each invocation asks for reaches the arm: `--no-arm` sends nothing,
/// the default is refused on an unapproved head, and `--arm` arms it and
/// names who did.
#[test]
fn each_arm_request_reaches_the_arm_as_asked() {
    use crate::app::cli::ArmRequest;
    let gh = |head: &str| {
        FakeGh::new(vec![
            ("pr view", Ok(pr_view("PR_9438", false))),
            ("isInMergeQueue", Ok(pr_9438_queue(head).to_string())),
            (
                "contents/.shipyard/config.toml",
                Ok(REQUIRES_APPROVAL.to_owned()),
            ),
            ("pulls/9438/reviews", Ok(String::new())),
            ("issues/9438/comments", Ok(String::new())),
            ("enablePullRequestAutoMerge", Ok(arm_accepted())),
        ])
    };
    let head = PR_9438_HEADS[0];

    let off = gh(head);
    assert!(arm_for_request(&|a| off.run(a), "o/r", 9438, false, ArmRequest::Off).is_none());
    assert_eq!(off.call_count(), 0);

    let default = gh(head);
    let outcome = arm_for_request(&|a| default.run(a), "o/r", 9438, false, ArmRequest::Default)
        .expect("attempted");
    assert!(!outcome.armed, "{}", outcome.line);

    let forced = gh(head);
    let outcome = arm_for_request(&|a| forced.run(a), "o/r", 9438, false, ArmRequest::Override)
        .expect("attempted");
    assert!(outcome.armed, "{}", outcome.line);
    assert!(
        outcome.line.contains("explicit `--arm` from "),
        "{}",
        outcome.line
    );
    assert!(outcome.line.contains('@'), "{}", outcome.line);
}

/// `ship`'s arm-on-open re-enqueues a starved head of #8678 at exactly that
/// head: the mutation carries `expectedHeadOid`, never the plain arm.
#[test]
fn a_same_head_reenqueue_from_ship_is_bound_to_the_exact_head() {
    let state = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/github/pr_real_8678_first_environment_ejection.json"
    ))
    .expect("fixture");
    let group = "2410ca497342cfc0264bf5b72713de0b56099a0f";
    let runs = r#"{"total_count":1,"check_runs":[{"id":7,"name":"macos","status":"completed","conclusion":"cancelled","app":{"slug":"github-actions"}}]}"#;
    let job = r#"{"id":7,"status":"completed","conclusion":"cancelled","created_at":"2026-09-23T03:30:00Z","completed_at":"2026-09-23T03:46:00Z","runner_name":"","steps":[]}"#;
    let gh = FakeGh::new(vec![
        ("pr view", Ok(pr_view("PR_node8678", false))),
        ("isInMergeQueue", Ok(state)),
        ("rules/branches/main", Ok("[]".to_owned())),
        (
            "protection/required_status_checks",
            Ok(r#"{"contexts":["macos"]}"#.to_owned()),
        ),
        ("pulls/8678", Ok(r#"{"base":{"ref":"main"}}"#.to_owned())),
        (
            Box::leak(format!("commits/{group}/check-runs").into_boxed_str()),
            Ok(runs.to_owned()),
        ),
        (
            Box::leak(format!("commits/{group}/status").into_boxed_str()),
            Ok(r#"{"statuses":[]}"#.to_owned()),
        ),
        ("check-runs/7/annotations", Ok("[]".to_owned())),
        ("actions/jobs/7", Ok(job.to_owned())),
        ("enablePullRequestAutoMerge", Ok(arm_accepted())),
    ]);
    let outcome =
        arm_native_auto_merge(&|args| gh.run(args), "Generous-Corp/pulp", 8678, true, None);
    assert!(outcome.armed, "{}", outcome.line);
    assert!(
        outcome.line.contains("at exactly its current head"),
        "{}",
        outcome.line
    );
    let calls = gh.calls.borrow();
    let arm = calls
        .iter()
        .find(|call| call.contains("enablePullRequestAutoMerge"))
        .expect("an arm mutation");
    assert!(arm.contains("expectedHeadOid:$head"), "{arm}");
    assert!(
        arm.contains("head=f0fb2fb38ef5c900efad7ea0630906f081db53a9"),
        "{arm}"
    );
}
