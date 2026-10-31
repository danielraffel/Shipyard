//! The steward's one automatic re-enqueue of a head the merge queue ejected
//! for a failure that is not the head's fault, through the real enqueue path.
use super::*;
use crate::app::merge_steward_cmd::pr_mutations::mutate_pr;

const GROUP: &str = "c0ffee0000000000000000000000000000000001";
const BASE_TIP: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

const OPTED_IN: &str = r"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = false' '[queue.environment_requeue]' 'enabled = true'";
const HOLD_POLICY: &str = r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["shipyard-review[bot]"]' '[queue.environment_requeue]' 'enabled = true'"#;

/// One `failed_checks` removal of head `aaaa…` from merge group [`GROUP`].
fn removal(minute: u32) -> String {
    format!(
        r#"{{"__typename":"AddedToMergeQueueEvent","createdAt":"2026-10-31T21:{minute:02}:00Z"}},{{"__typename":"RemovedFromMergeQueueEvent","createdAt":"2026-10-31T21:{:02}:00Z","reason":"failed_checks","beforeCommit":{{"oid":"{GROUP}","parents":{{"nodes":[{{"oid":"{BASE_TIP}"}},{{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}]}}}}}}"#,
        minute + 10
    )
}

/// A fake `gh` for one steward pass over PR 42: `config` answers the base's
/// Shipyard config, `ejections` removals of the unchanged head, and the
/// merge group's required `macos` check concluded `conclusion`.
fn script(log: &Path, config: &str, ejections: u32, conclusion: &str, comments: &str) -> String {
    let removals = (0..ejections)
        .map(|index| removal(index * 20))
        .collect::<Vec<_>>()
        .join(",");
    r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '__LOG__'
case "$*" in
  *"contents/.shipyard/config.toml"*) __CONFIG__ ;;
  *"isInMergeQueue"*) printf '%s' '{"data":{"repository":{"pullRequest":{"number":42,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[{"__typename":"PullRequestCommit","commit":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}},__REMOVALS__]}}}}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  "pr view "*)
    printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}]}' ;;
  *"pulls/42/reviews"*) printf '' ;;
  *"issues/42/comments"*) printf '%s\n' '__COMMENTS__' ;;
  *"commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"*) printf '%s' '2026-10-31T00:00:00Z' ;;
  *"repos/owner/repo/pulls/42") printf '%s' '{"base":{"ref":"main"}}' ;;
  *"rules/branches/main"*) printf '%s' '[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"macos"}]}}]' ;;
  *"protection/required_status_checks"*) echo 'HTTP 404: Not Found' >&2; exit 1 ;;
  *"commits/__GROUP__/check-runs"*) printf '%s' '{"total_count":1,"check_runs":[{"id":7,"name":"macos","status":"completed","conclusion":"__CONCLUSION__","app":{"slug":"github-actions"}}]}' ;;
  *"commits/__GROUP__/status"*) printf '%s' '{"statuses":[]}' ;;
  *"stackConfig"*) printf '%s' '{"data":{"repository":{"stackConfig":null,"pullRequest":{"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stack":null,"stackEntry":null}}}}' ;;
  *"stackEntry"*) printf '%s' '{"data":{"repository":{"pullRequest":{"stack":null,"stackEntry":null}}}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"position":1}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#
    .replace("__LOG__", &log.display().to_string())
    .replace("__CONFIG__", config)
    .replace("__REMOVALS__", &removals)
    .replace("__GROUP__", GROUP)
    .replace("__CONCLUSION__", conclusion)
    .replace("__COMMENTS__", comments)
}

struct Pass {
    mutation: Option<String>,
    error: Option<String>,
    calls: String,
    retry: Option<crate::app::merge_steward_cmd::blocked_on::SameHeadRetry>,
}

fn pass(temp: &tempfile::TempDir, body: &str) -> Pass {
    let log = temp.path().join("calls");
    let actions = fake_gh(temp, body);
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let control = mutation_control(temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );
    let retry = context
        .approval_blockers
        .borrow()
        .get(&42)
        .and_then(|record| record.ejection.as_ref())
        .and_then(|ejection| ejection.retry.clone());
    Pass {
        mutation,
        error,
        calls: fs::read_to_string(log).unwrap_or_default(),
        retry,
    }
}

#[cfg(unix)]
#[test]
fn an_infra_ejection_is_re_enqueued_once_at_the_same_head() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(&temp.path().join("calls"), OPTED_IN, 1, "timed_out", "");
    let pass = pass(&temp, &body);
    assert!(pass.error.is_none(), "{:?}\n{}", pass.error, pass.calls);
    assert!(
        pass.calls.contains("enqueuePullRequest"),
        "{:?}\n{}",
        pass.mutation,
        pass.calls
    );
    assert!(
        pass.mutation
            .as_deref()
            .is_some_and(|mutation| mutation.starts_with("enqueued")),
        "{:?}",
        pass.mutation
    );
    let retry = pass.retry.expect("retry verdict recorded");
    assert!(retry.allowed, "{}", retry.detail);
    // The enqueue is the exact-head one: bound to the head the gate read.
    assert!(
        pass.calls
            .lines()
            .any(|line| line.contains("enqueuePullRequest")
                && line.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
        "{}",
        pass.calls
    );
}

#[cfg(unix)]
#[test]
fn a_second_ejection_of_the_same_head_is_left_for_repair_and_says_so() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(&temp.path().join("calls"), OPTED_IN, 2, "timed_out", "");
    let pass = pass(&temp, &body);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("skipped_ejected_same_head:failed_checks"),
        "{:?}\n{}",
        pass.error,
        pass.calls
    );
    assert!(pass.error.is_none(), "{:?}", pass.error);
    assert!(!pass.calls.contains("enqueuePullRequest"), "{}", pass.calls);
    let retry = pass.retry.expect("retry verdict recorded");
    assert!(!retry.allowed);
    assert!(retry.detail.contains("has been spent"), "{}", retry.detail);
    assert!(retry.detail.contains("`macos`"), "{}", retry.detail);
}

#[cfg(unix)]
#[test]
fn an_unplaceable_ejection_is_not_re_enqueued() {
    // A plain `failure` whose job, log, base tip and head nobody answers.
    let temp = tempfile::tempdir().expect("temp");
    let body = script(&temp.path().join("calls"), OPTED_IN, 1, "failure", "");
    let pass = pass(&temp, &body);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("skipped_ejected_same_head:failed_checks"),
        "{:?}\n{}",
        pass.error,
        pass.calls
    );
    assert!(!pass.calls.contains("enqueuePullRequest"), "{}", pass.calls);
    let retry = pass.retry.expect("retry verdict recorded");
    assert!(!retry.allowed);
    assert!(
        retry.detail.contains("not cleared of the head"),
        "{}",
        retry.detail
    );
}

#[cfg(unix)]
#[test]
fn a_hold_on_the_head_blocks_the_re_enqueue() {
    let temp = tempfile::tempdir().expect("temp");
    let hold = r#"{"id":12345,"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-31T22:00:00Z","body":"verdict: HOLD head:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","performed_via_github_app_id":99}"#;
    let body = script(
        &temp.path().join("calls"),
        HOLD_POLICY,
        1,
        "timed_out",
        hold,
    );
    let pass = pass(&temp, &body);
    assert!(!pass.calls.contains("enqueuePullRequest"), "{}", pass.calls);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("waiting_head_approval"),
        "{:?}\n{}",
        pass.error,
        pass.calls
    );
    // Control: the same ejection with the HOLD removed and approval not
    // required is re-enqueued, so the HOLD is what stopped it.
    let temp = tempfile::tempdir().expect("temp");
    let body = script(&temp.path().join("calls"), OPTED_IN, 1, "timed_out", "");
    assert!(pass_enqueues(&temp, &body));
}

#[cfg(unix)]
fn pass_enqueues(temp: &tempfile::TempDir, body: &str) -> bool {
    pass(temp, body).calls.contains("enqueuePullRequest")
}

#[cfg(unix)]
#[test]
fn a_repository_that_has_not_opted_in_keeps_the_refusal() {
    let temp = tempfile::tempdir().expect("temp");
    let config = r"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = false'";
    let body = script(&temp.path().join("calls"), config, 1, "timed_out", "");
    let pass = pass(&temp, &body);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("skipped_ejected_same_head:failed_checks")
    );
    assert!(!pass.calls.contains("enqueuePullRequest"), "{}", pass.calls);
}
