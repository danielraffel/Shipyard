//! Reviewer-side R1/R8 probes through the real enqueue entry point.
use super::*;
use crate::app::merge_steward_cmd::pr_mutations::enqueue_pull_request;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EARLIER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn script(log: &Path, config_case: &str, comments_json: &str) -> String {
    // The literal CONFIG_ERROR below disables fake_gh's absent-policy prefix.
    format!(
        r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/.shipyard/config.toml"*) {config_case} ;;
  *"pulls/42/reviews"*) printf '' ;;
  *"issues/42/comments"*) printf '%s\n' '{comments}' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"headRefOid":"{head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"stackEntry"*) echo "GATE_PASSED_REACHED_STACK" >&2; exit 90 ;;
  *"enqueuePullRequest"*) echo "mutation must not run" >&2; exit 91 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        log = log.display(),
        config_case = config_case,
        comments = comments_json,
        head = HEAD,
    )
}

const POLICY_OK: &str = r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_logins = ["reviewer"]'"#;
const POLICY_ERR: &str = r#"echo 'HTTP 500 server error' >&2; exit 1"#;

fn comment(login: &str, body: &str) -> String {
    format!(
        r#"{{"id":12345,"login":"{login}","type":"User","at":"2026-10-11T12:00:00Z","body":"{body}"}}"#
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
    let policy = queue_policy();
    let (mutation, error) = enqueue_pull_request(&context, &pr, &policy, &mut ledger);
    let calls = fs::read_to_string(log).unwrap_or_default();
    (mutation, error, calls)
}

fn assert_held(mutation: &Option<String>, error: &Option<String>, calls: &str) {
    assert!(
        !calls.contains("stackEntry")
            && !calls.contains("stackConfig")
            && !calls.contains("enqueuePullRequest")
            && mutation.as_deref() != None,
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
        &comment("reviewer", &format!("LGTM\\nreviewed:{HEAD}")),
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

#[cfg(unix)]
#[test]
fn r774_r8_marker_naming_an_earlier_head_does_not_arm() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        POLICY_OK,
        &comment("reviewer", &format!("LGTM\\nreviewed:{EARLIER}")),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_held(&mutation, &error, &calls);
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
    assert_held(&mutation, &error, &calls);
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
        &comment("reviewer", &format!("reviewed:{HEAD}")),
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
    let comments = format!(
        "{}
{}",
        comment("reviewer", &format!("LGTM\\nreviewed:{HEAD}")),
        r#"{"login":"reviewer","type":"User","at":"2026-10-11T13:00:00Z","body":"HOLD. regression found on this head, do not arm."}"#
    );
    let body = script(&log, POLICY_OK, &comments);
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_held(&mutation, &error, &calls);
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
            "reviewer",
            &format!("HOLD. must-fix list follows.\\nreviewed:{HEAD}"),
        ),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_held(&mutation, &error, &calls);
}

#[cfg(unix)]
#[test]
fn r774_r1_author_cannot_approve_own_head() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        POLICY_OK,
        &comment("reviewer", &format!("reviewed:{HEAD}")),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("reviewer"));
    assert_held(&mutation, &error, &calls);
}

#[cfg(unix)]
#[test]
fn r774_r1_short_prefix_marker_does_not_arm() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        POLICY_OK,
        &comment("reviewer", &format!("reviewed:{}", &HEAD[..7])),
    );
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_held(&mutation, &error, &calls);
}

#[cfg(unix)]
#[test]
fn r774_r1_absent_policy_file_does_not_arm_unapproved_head() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(&log, r#"echo '404 Not Found' >&2; exit 1"#, "");
    let (mutation, error, calls) = run(&temp, &body, Some("shipyard-local[bot]"));
    assert_held(&mutation, &error, &calls);
}
