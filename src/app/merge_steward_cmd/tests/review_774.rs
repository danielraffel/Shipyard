//! Reviewer-side R1/R8 probes through the real enqueue entry point.
use super::*;
use crate::app::merge_steward_cmd::pr_mutations::enqueue_pull_request;
use crate::merge_steward::StewardCheckSource;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EARLIER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn script(log: &Path, config_case: &str, comments_json: &str) -> String {
    script_with_reviews(log, config_case, "", comments_json)
}

/// [`script`], also serving `reviews_json` (one projected review per line)
/// for the PR's reviews.
fn script_with_reviews(
    log: &Path,
    config_case: &str,
    reviews_json: &str,
    comments_json: &str,
) -> String {
    // The literal CONFIG_ERROR below disables fake_gh's absent-policy prefix.
    format!(
        r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/.shipyard/config.toml"*) {config_case} ;;
  *"pulls/42/reviews"*) printf '%s\n' '{reviews}' ;;
  *"issues/42/comments"*) printf '%s\n' '{comments}' ;;
  *"commits/{head}"*) printf '%s' '2026-10-11T00:00:00Z' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"headRefOid":"{head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"stackEntry"*) echo "GATE_PASSED_REACHED_STACK" >&2; exit 90 ;;
  *"enqueuePullRequest"*) echo "mutation must not run" >&2; exit 91 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        log = log.display(),
        config_case = config_case,
        comments = comments_json,
        reviews = reviews_json,
        head = HEAD,
    )
}

const POLICY_OK: &str = r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["shipyard-review[bot]", "danielraffel"]'"#;
const POLICY_ERR: &str = r"echo 'HTTP 500 server error' >&2; exit 1";

fn comment(login: &str, body: &str) -> String {
    format!(
        r#"{{"id":12345,"login":"{login}","type":"User","at":"2026-10-11T12:00:00Z","body":"{body}","performed_via_github_app_id":99}}"#
    )
}

fn run(
    temp: &tempfile::TempDir,
    body: &str,
    author: Option<&str>,
) -> (Option<String>, Option<String>, String) {
    let log = temp.path().join("calls");
    let actions = fake_gh(temp, body);
    let mut pr = ready_pr();
    if let Some(author) = author {
        pr.fact.author = Some(author.to_owned());
    }
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);
    let mut policy = queue_policy();
    policy.managed_label = Some("steward:managed".to_owned());
    let (mutation, error) = enqueue_pull_request(&context, &pr, &policy, &mut ledger);
    let calls = fs::read_to_string(log).unwrap_or_default();
    (mutation, error, calls)
}

fn assert_held(mutation: Option<&String>, error: Option<&String>, calls: &str) {
    assert!(
        !calls.contains("stackEntry")
            && !calls.contains("stackConfig")
            && !calls.contains("enqueuePullRequest")
            && mutation.is_some(),
        "gate let the enqueue path continue: mutation={mutation:?} error={error:?}\n{calls}"
    );
}

#[cfg(unix)]
#[test]
fn r774_positive_exact_head_marker_passes_the_gate() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        POLICY_OK,
        &comment("shipyard-review[bot]", &format!("LGTM\\nreviewed: {HEAD}")),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert!(
        calls.contains("stackEntry"),
        "gate did not pass: {mutation:?} {error:?}\n{calls}"
    );
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.contains("GATE_PASSED_REACHED_STACK")),
        "{error:?}"
    );
}

/// Drive `enqueue_pull_request` all the way to the enqueue mutation and
/// return the mutation audit it wrote.
fn enqueue_audit(temp: &tempfile::TempDir, reviews_json: &str, comments_json: &str) -> String {
    let log = temp.path().join("calls");
    let body = script_with_reviews(&log, POLICY_OK, reviews_json, comments_json)
    .replace(
        r#"*"stackEntry"*) echo "GATE_PASSED_REACHED_STACK" >&2; exit 90 ;;"#,
        r#"*"stackEntry"*) printf '%s' '{"data":{"repository":{"stackConfig":null,"pullRequest":{"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stack":null,"stackEntry":null}}}}' ;;
  "pr view "*) printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{"name":"steward:managed"}],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-10-11T12:00:00Z"},{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-10-11T12:00:01Z"}]}' ;;
  *"pulls/42"*) printf '%s' '{"state":"open","head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{}' ;;"#,
    );
    let actions = fake_gh(temp, &body);
    let mut pr = ready_pr();
    pr.fact.labels.push("steward:managed".to_owned());
    pr.fact.checks.push(StewardCheck {
        name: "shipyard/steward-handoff".to_owned(),
        source: StewardCheckSource::StatusContext,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: None,
        observed_at: Some("2026-10-11T12:00:01Z".to_owned()),
    });
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let control = mutation_control(temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let mut policy = queue_policy();
    policy.managed_label = Some("steward:managed".to_owned());
    let (_mutation, _error) = enqueue_pull_request(&context, &pr, &policy, &mut ledger);
    fs::read_to_string(temp.path().join("state/merge_queue/mutations.jsonl")).unwrap_or_default()
}

#[cfg(unix)]
#[test]
fn r774_enqueue_persists_marker_id_in_intent_and_receipt() {
    let temp = tempfile::tempdir().expect("temp");
    let audit = enqueue_audit(
        &temp,
        "",
        &comment("shipyard-review[bot]", &format!("reviewed: {HEAD}")),
    );
    assert!(audit.contains("\"approval_comment_id\":12345"), "{audit}");
    assert!(
        audit.contains("\"performed_via_github_app_id\":99"),
        "{audit}"
    );
}

#[cfg(unix)]
#[test]
fn r774_r8_marker_naming_an_earlier_head_does_not_arm() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        POLICY_OK,
        &comment(
            "shipyard-review[bot]",
            &format!("LGTM\\nreviewed:{EARLIER}"),
        ),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_held(mutation.as_ref(), error.as_ref(), &calls);
    assert_eq!(
        mutation.as_deref(),
        Some("waiting_head_approval"),
        "{error:?}"
    );
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.contains("carries no approval")),
        "{error:?}"
    );
}

#[cfg(unix)]
#[test]
fn r774_missing_approval_does_not_arm() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(&log, POLICY_OK, "");
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_held(mutation.as_ref(), error.as_ref(), &calls);
    assert_eq!(
        mutation.as_deref(),
        Some("waiting_head_approval"),
        "{error:?}"
    );
}

#[cfg(unix)]
#[test]
fn r774_policy_read_error_fails_closed() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        POLICY_ERR,
        &comment("shipyard-review[bot]", &format!("reviewed:{HEAD}")),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert!(
        !calls.contains("stackEntry") && !calls.contains("enqueuePullRequest"),
        "gate let the enqueue path continue: {mutation:?} {error:?}\n{calls}"
    );
    assert!(mutation.is_none(), "{mutation:?}");
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.contains("head approval policy unreadable")),
        "{error:?}"
    );
}

#[cfg(unix)]
#[test]
fn r774_r1_later_hold_by_a_recognized_reviewer_does_not_arm() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let hold = format!(
        r#"{{"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-11T13:00:00Z","body":"verdict: HOLD head:{HEAD}"}}"#
    );
    let comments = format!(
        "{}\n{hold}",
        comment("shipyard-review[bot]", &format!("LGTM\\nreviewed:{HEAD}")),
    );
    let body = script(&log, POLICY_OK, &comments);
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_held(mutation.as_ref(), error.as_ref(), &calls);
}

#[cfg(unix)]
#[test]
fn r774_r1_hold_comment_carrying_a_marker_line_does_not_arm() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        POLICY_OK,
        &comment(
            "shipyard-review[bot]",
            &format!("verdict: HOLD head:{HEAD}"),
        ),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_held(mutation.as_ref(), error.as_ref(), &calls);
}

#[cfg(unix)]
#[test]
fn r774_r1_author_cannot_approve_own_head() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        POLICY_OK,
        &comment("shipyard-review[bot]", &format!("reviewed:{HEAD}")),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-review[bot]"));
    assert_held(mutation.as_ref(), error.as_ref(), &calls);
}

#[cfg(unix)]
#[test]
fn r774_r1_short_prefix_marker_does_not_arm() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        POLICY_OK,
        &comment("shipyard-review[bot]", &format!("reviewed:{}", &HEAD[..7])),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_held(mutation.as_ref(), error.as_ref(), &calls);
}

#[cfg(unix)]
#[test]
fn r774_r1_absent_policy_file_does_not_arm_unapproved_head() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = format!(
        "# CONFIG_ERROR\n{}",
        script(&log, r"echo '404 Not Found' >&2; exit 1", "",)
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert!(!calls.contains("stackEntry") && !calls.contains("enqueuePullRequest"));
    assert!(mutation.is_none());
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.contains("policy unreadable"))
    );
}

fn comment_at(login: &str, at: &str, body: &str) -> String {
    format!(r#"{{"id":777,"login":"{login}","type":"Bot","at":"{at}","body":"{body}"}}"#)
}

fn review_record(login: &str, state: &str, commit: &str, at: &str) -> String {
    format!(
        r#"{{"login":"{login}","type":"Bot","at":"{at}","state":"{state}","commit_id":"{commit}","performed_via_github_app_id":4242}}"#
    )
}

fn assert_gate_passed(mutation: Option<&String>, error: Option<&String>, calls: &str) {
    assert!(
        calls.contains("stackEntry")
            && error.is_some_and(|e| e.contains("GATE_PASSED_REACHED_STACK")),
        "gate did not pass: {mutation:?} {error:?}\n{calls}"
    );
}

#[cfg(unix)]
#[test]
fn r774_hold_on_an_earlier_head_does_not_block_an_approved_head() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let comments = [
        comment_at(
            "shipyard-review[bot]",
            "2026-10-11T11:00:00Z",
            &format!("verdict: HOLD head:{EARLIER}"),
        ),
        comment_at(
            "shipyard-review[bot]",
            "2026-10-11T12:00:00Z",
            &format!("verdict: APPROVE head:{HEAD}"),
        ),
        comment_at(
            "shipyard-review[bot]",
            "2026-10-11T13:00:00Z",
            &format!("verdict: HOLD head:{EARLIER}"),
        ),
    ]
    .join("\n");
    let body = script(&log, POLICY_OK, &comments);
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_gate_passed(mutation.as_ref(), error.as_ref(), &calls);
}

#[cfg(unix)]
#[test]
fn r774_marker_older_than_the_head_commit_does_not_arm() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    // The fixture's head commit is dated 2026-10-11T00:00:00Z.
    let body = script(
        &log,
        POLICY_OK,
        &comment_at(
            "shipyard-review[bot]",
            "2026-10-10T23:59:59Z",
            &format!("reviewed:{HEAD}"),
        ),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert!(
        calls.contains("commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        "the head commit date was read: {calls}"
    );
    assert_held(mutation.as_ref(), error.as_ref(), &calls);
    assert_eq!(mutation.as_deref(), Some("waiting_head_approval"));
}

#[cfg(unix)]
#[test]
fn r774_review_app_approve_on_the_head_arms_and_records_the_app() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let reviews = review_record(
        "shipyard-review[bot]",
        "APPROVED",
        HEAD,
        "2026-10-11T12:00:00Z",
    );
    let body = script_with_reviews(&log, POLICY_OK, &reviews, "");
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_gate_passed(mutation.as_ref(), error.as_ref(), &calls);

    let temp = tempfile::tempdir().expect("temp");
    let audit = enqueue_audit(&temp, &reviews, "");
    assert!(
        audit.contains("\"performed_via_github_app_id\":4242"),
        "{audit}"
    );
}

#[cfg(unix)]
#[test]
fn r774_review_approvals_that_must_not_arm() {
    for (case, reviews) in [
        (
            "fleet author App",
            review_record(
                "app/shipyard-local",
                "APPROVED",
                HEAD,
                "2026-10-11T12:00:00Z",
            ),
        ),
        (
            "earlier head",
            review_record(
                "shipyard-review[bot]",
                "APPROVED",
                EARLIER,
                "2026-10-11T12:00:00Z",
            ),
        ),
        (
            "changes requested after the approval",
            [
                review_record(
                    "shipyard-review[bot]",
                    "APPROVED",
                    HEAD,
                    "2026-10-11T12:00:00Z",
                ),
                review_record(
                    "shipyard-review[bot]",
                    "CHANGES_REQUESTED",
                    HEAD,
                    "2026-10-11T13:00:00Z",
                ),
            ]
            .join("\n"),
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let body = script_with_reviews(&log, POLICY_OK, &reviews, "");
        let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
        assert_held(mutation.as_ref(), error.as_ref(), &calls);
        assert_eq!(
            mutation.as_deref(),
            Some("waiting_head_approval"),
            "{case}: {error:?}"
        );
    }
}

/// Fixture for the apply-mode `mutate_pr` entry point: serves the full
/// live-revalidation surface so the only difference between cases is the
/// approval evidence. `comments_case` is a shell fragment printing the
/// comments for each `issues/42/comments` read.
fn apply_script(log: &Path, comments_case: &str) -> String {
    script_with_reviews(log, POLICY_OK, "", "")
        .replace(r#""number":42,"headRefOid""#, r#""number":42,"state":"OPEN","headRefOid""#)
        .replace(
            r#"*"issues/42/comments"*) printf '%s\n' '' ;;"#,
            &format!(r#"*"issues/42/comments"*) {comments_case} ;;"#),
        )
        .replace(
            r#"*"stackEntry"*) echo "GATE_PASSED_REACHED_STACK" >&2; exit 90 ;;"#,
            r#"*"mergeQueue(branch"*) printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{"data":{"repository":{"stackConfig":null,"pullRequest":{"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stack":null,"stackEntry":null}}}}' ;;
  "pr view "*) printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{"name":"steward:managed"}],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-10-11T12:00:00Z"},{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-10-11T12:00:01Z"}]}' ;;
  *"pulls/42"*) printf '%s' '{"state":"open","head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{}' ;;"#,
        )
}

fn apply_run(temp: &tempfile::TempDir, body: &str) -> (Option<String>, Option<String>, String) {
    let actions = fake_gh(temp, body);
    let mut pr = ready_pr();
    pr.fact.author = Some("shipyard-local[bot]".to_owned());
    pr.fact.labels.push("steward:managed".to_owned());
    pr.fact.checks.push(StewardCheck {
        name: "shipyard/steward-handoff".to_owned(),
        source: StewardCheckSource::StatusContext,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: None,
        observed_at: Some("2026-10-11T12:00:01Z".to_owned()),
    });
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let control = mutation_control(temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let mut policy = queue_policy();
    policy.managed_label = Some("steward:managed".to_owned());
    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );
    let calls = fs::read_to_string(temp.path().join("calls")).unwrap_or_default();
    (mutation, error, calls)
}

fn count(calls: &str, needle: &str) -> usize {
    calls.lines().filter(|line| line.contains(needle)).count()
}

#[cfg(unix)]
#[test]
fn apply_unapproved_arm_candidate_skips_live_revalidation() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = apply_script(
        &log,
        &format!(
            "printf '%s\\n' '{}'",
            comment_at(
                "shipyard-review[bot]",
                "2026-10-11T12:00:00Z",
                &format!("verdict: HOLD head:{HEAD}"),
            )
        ),
    );
    let (mutation, error, calls) = apply_run(&temp, &body);
    assert_eq!(
        mutation.as_deref(),
        Some("waiting_head_approval"),
        "{error:?}\n{calls}"
    );
    assert_eq!(count(&calls, "pr view"), 0, "{calls}");
    assert_eq!(
        count(&calls, "mergeQueue(branch"),
        0,
        "no queue snapshot: {calls}"
    );
    assert_eq!(
        count(&calls, "pulls/42\n") + count(&calls, "enqueuePullRequest"),
        0,
        "{calls}"
    );
    // One policy read, one queue-state read, one comments read: nothing else.
    assert_eq!(count(&calls, "issues/42/comments"), 1, "{calls}");
}

#[cfg(unix)]
#[test]
fn apply_approved_arm_candidate_still_revalidates_and_enqueues() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = apply_script(
        &log,
        &format!(
            "printf '%s\\n' '{}'",
            comment_at(
                "shipyard-review[bot]",
                "2026-10-11T12:00:00Z",
                &format!("verdict: APPROVE head:{HEAD}"),
            )
        ),
    );
    let (mutation, error, calls) = apply_run(&temp, &body);
    assert!(count(&calls, "pr view") >= 1, "live re-read ran: {calls}");
    assert!(count(&calls, "mergeQueue(branch") >= 1, "{calls}");
    assert!(
        count(&calls, "enqueuePullRequest") >= 1,
        "reached enqueue: {mutation:?} {error:?}\n{calls}"
    );
    // Approval is re-read inside enqueue, on the live head.
    assert_eq!(count(&calls, "issues/42/comments"), 2, "{calls}");
}

#[cfg(unix)]
#[test]
fn apply_hold_posted_after_census_is_still_refused_inside_enqueue() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let approve = comment_at(
        "shipyard-review[bot]",
        "2026-10-11T12:00:00Z",
        &format!("verdict: APPROVE head:{HEAD}"),
    );
    let hold = comment_at(
        "shipyard-review[bot]",
        "2026-10-11T13:00:00Z",
        &format!("verdict: HOLD head:{HEAD}"),
    );
    let seen = temp.path().join("seen-first-read");
    let case = format!(
        "if [ -e '{seen}' ]; then printf '%s\\n%s\\n' '{approve}' '{hold}'; else : > '{seen}'; printf '%s\\n' '{approve}'; fi",
        seen = seen.display(),
    );
    let body = apply_script(&log, &case);
    let (mutation, error, calls) = apply_run(&temp, &body);
    assert_eq!(
        mutation.as_deref(),
        Some("waiting_head_approval"),
        "{error:?}\n{calls}"
    );
    assert!(
        count(&calls, "pr view") >= 1,
        "pre-filter passed, live path ran: {calls}"
    );
    assert_eq!(count(&calls, "enqueuePullRequest"), 0, "{calls}");
}
