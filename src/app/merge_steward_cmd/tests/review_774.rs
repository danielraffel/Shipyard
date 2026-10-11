//! Reviewer-side R1/R8 probes through the real enqueue entry point.
use super::*;
use crate::app::merge_steward_cmd::pr_mutations::enqueue_pull_request;
use crate::merge_steward::StewardCheckSource;

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

#[cfg(unix)]
#[test]
fn r774_enqueue_persists_marker_id_in_intent_and_receipt() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        POLICY_OK,
        &comment("shipyard-review[bot]", &format!("reviewed: {HEAD}")),
    )
    .replace(
        r#"*"stackEntry"*) echo "GATE_PASSED_REACHED_STACK" >&2; exit 90 ;;"#,
        r#"*"stackEntry"*) printf '%s' '{"data":{"repository":{"stackConfig":null,"pullRequest":{"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stack":null,"stackEntry":null}}}}' ;;
  "pr view "*) printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{"name":"steward:managed"}],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-10-11T12:00:00Z"},{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-10-11T12:00:01Z"}]}' ;;
  *"pulls/42"*) printf '%s' '{"state":"open","head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{}' ;;"#,
    );
    let actions = fake_gh(&temp, &body);
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
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let mut policy = queue_policy();
    policy.managed_label = Some("steward:managed".to_owned());
    let (_mutation, _error) = enqueue_pull_request(&context, &pr, &policy, &mut ledger);
    let audit = fs::read_to_string(temp.path().join("state/merge_queue/mutations.jsonl"))
        .expect("mutation audit");
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
    let comments = format!(
        "{}
{}",
        comment("shipyard-review[bot]", &format!("LGTM\\nreviewed:{HEAD}")),
        r#"{{"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-11T13:00:00Z","body":"verdict: HOLD head:{HEAD}"}}"#
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
