//! `shipyard wait pr <n> --state actionable`'s snapshot, end to end over a
//! fake `gh`: one pull request read, the same admission dry run the steward
//! runs, and the next action an agent follows.
use super::*;
use crate::app::merge_steward_cmd::pr_next_action_snapshot;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The repository and policy answers every scenario shares: a merge-queue
/// base whose rules require `macos` and `review-verdict` from GitHub Actions.
const REPO_CASES: &str = r#"
  *"api repos/owner/repo") printf '%s' '{"full_name":"owner/repo","allow_auto_merge":true,"default_branch":"main"}' ;;
  *"rules/branches/main"*) printf '%s' '[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"macos","integration_id":15368},{"context":"review-verdict","integration_id":15368}]}}]' ;;
  *"branches/main/protection/required_status_checks"*) echo 'HTTP 404: Not Found' >&2; exit 1 ;;
  *"mergeQueue(branch"*) printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  *"isInMergeQueue"*) printf '%s' '{"data":{"repository":{"pullRequest":{"number":42,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[]}}}}}' ;;
  *"contents/.shipyard/config.toml"*) printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["shipyard-review[bot]"]' ;;
  *"pulls/42/reviews"*) ;;
  *"commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/check-runs"*) printf '%s' '{"total_count":0,"check_runs":[]}' ;;
  *"commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/statuses"*) printf '%s' '[]' ;;
  *"commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"*"--jq"*) printf '%s\n' '2026-11-01T09:00:00Z' ;;
  *"commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"*) printf '%s' '{"commit":{"committer":{"date":"2026-11-01T09:00:00Z"}}}' ;;
  *"pulls/42"*"--jq"*) printf '%s\n' 'shipyard-local[bot]' ;;
"#;

fn check(name: &str, conclusion: &str, at: &str) -> String {
    format!(
        r#"{{"__typename":"CheckRun","name":"{name}","status":"COMPLETED","conclusion":"{conclusion}","startedAt":"{at}","completedAt":"{at}","detailsUrl":"https://github.com/owner/repo/actions/runs/100","checkSuite":{{"app":{{"databaseId":15368}}}}}}"#
    )
}

/// A `gh pr view` answer with [`observation::PR_VIEW_FIELDS`].
fn pr_row(state: &str, base: &str, merge_state: &str, changed_files: u64, rollup: &str) -> String {
    format!(
        r#"{{"id":"PR_kw","number":42,"state":"{state}","isDraft":false,"baseRefName":"{base}","headRefOid":"{HEAD}","headRefName":"feature","mergeStateStatus":"{merge_state}","autoMergeRequest":null,"labels":[],"author":{{"login":"app/shipyard-local"}},"changedFiles":{changed_files},"mergedAt":{merged_at},"statusCheckRollup":[{rollup}]}}"#,
        merged_at = if state == "MERGED" {
            r#""2026-11-01T10:00:00Z""#
        } else {
            "null"
        }
    )
}

fn script(log: &Path, pr_row: &str, comments: &str, extra_cases: &str) -> String {
    format!(
        r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  "pr view 42 --repo owner/repo --json "*) printf '%s' '{pr_row}' ;;
  *"issues/42/comments"*) printf '%s\n' '{comments}' ;;
{extra_cases}{REPO_CASES}
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        log = log.display(),
    )
}

fn read_snapshot(temp: &tempfile::TempDir, body: &str) -> (Value, String) {
    let log = temp.path().join("calls");
    let actions = fake_gh(temp, body);
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).expect("state dir");
    let snapshot = crate::head_approval::with_host_review_default(true, || {
        pr_next_action_snapshot(&actions, &state_dir, "owner/repo", 42)
    })
    .expect("snapshot");
    let calls = fs::read_to_string(log).unwrap_or_default();
    (snapshot, calls)
}

fn green_rollup() -> String {
    format!(
        "{},{}",
        check("macos", "SUCCESS", "2026-11-01T10:00:00Z"),
        check("review-verdict", "FAILURE", "2026-11-01T10:05:00Z")
    )
}

#[cfg(unix)]
#[test]
fn a_hold_at_the_exact_head_is_the_authors_move_with_the_record_linked() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let hold = format!(
        r#"{{"id":6544000001,"login":"shipyard-review[bot]","type":"Bot","at":"2026-11-01T10:10:00Z","body":"verdict: HOLD head:{HEAD}","performed_via_github_app_id":99}}"#
    );
    let body = script(
        &log,
        &pr_row("OPEN", "main", "CLEAN", 3, &green_rollup()),
        &hold,
        "",
    );
    let (snapshot, calls) = read_snapshot(&temp, &body);
    assert_eq!(snapshot["blocked_on"], "held", "{snapshot:#}\n{calls}");
    assert_eq!(snapshot["actionable"], true);
    assert_eq!(snapshot["next_action"]["actor"], "author");
    assert_eq!(snapshot["next_action"]["stop"], false);
    let action = snapshot["next_action"]["action"]
        .as_str()
        .expect("action text");
    assert!(action.contains("HOLD by shipyard-review[bot]"), "{action}");
    assert!(
        action.contains("https://github.com/owner/repo/pull/42#issuecomment-6544000001"),
        "{action}"
    );
    assert_eq!(snapshot["held_by"]["holder"], "shipyard-review[bot]");
    assert_eq!(snapshot["state"], "OPEN");
    assert_eq!(snapshot["headRefOid"], HEAD);
    assert_eq!(snapshot["required_contexts"][0], "macos (app_id=15368)");
    assert!(
        !calls.contains("enqueuePullRequest") && !calls.contains("-X POST"),
        "a snapshot must not mutate:\n{calls}"
    );
}

#[cfg(unix)]
#[test]
fn green_with_no_verdict_is_a_wait_not_an_action() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        &pr_row("OPEN", "main", "BEHIND", 3, &green_rollup()),
        "",
        "",
    );
    let (snapshot, calls) = read_snapshot(&temp, &body);
    assert_eq!(
        snapshot["blocked_on"], "no_verdict",
        "{snapshot:#}\n{calls}"
    );
    assert_eq!(snapshot["actionable"], false);
    assert_eq!(snapshot["next_action"]["actor"], "reviewer");
    let action = snapshot["next_action"]["action"]
        .as_str()
        .expect("action text");
    assert!(action.contains("nothing for the author to do"), "{action}");
    // BEHIND on a merge-queue base is never a reason to merge main.
    assert!(action.contains("do not merge main"), "{action}");
    assert_eq!(snapshot["merge_state"], "BEHIND");
}

#[cfg(unix)]
#[test]
fn a_failed_required_check_names_the_check() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let rollup = format!(
        "{},{}",
        check("macos", "FAILURE", "2026-11-01T10:00:00Z"),
        check("review-verdict", "FAILURE", "2026-11-01T10:05:00Z")
    );
    let body = script(&log, &pr_row("OPEN", "main", "CLEAN", 3, &rollup), "", "");
    let (snapshot, calls) = read_snapshot(&temp, &body);
    assert_eq!(
        snapshot["blocked_on"], "required_failed:macos,review-verdict",
        "{snapshot:#}\n{calls}"
    );
    assert_eq!(snapshot["actionable"], true);
    assert_eq!(snapshot["next_action"]["actor"], "author");
    let action = snapshot["next_action"]["action"]
        .as_str()
        .expect("action text");
    assert!(
        action.contains("shipyard diagnose --pr 42 --repo owner/repo"),
        "{action}"
    );
}

#[cfg(unix)]
#[test]
fn a_merged_pull_request_stops_the_caller_before_any_admission_read() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(&log, &pr_row("MERGED", "main", "UNKNOWN", 3, ""), "", "");
    let (snapshot, calls) = read_snapshot(&temp, &body);
    assert_eq!(snapshot["blocked_on"], "merged", "{snapshot:#}");
    assert_eq!(snapshot["merged"], true);
    assert_eq!(snapshot["actionable"], true);
    assert_eq!(snapshot["next_action"]["stop"], true);
    assert_eq!(snapshot["next_action"]["actor"], "nobody");
    let action = snapshot["next_action"]["action"]
        .as_str()
        .expect("action text");
    assert!(action.contains("2026-11-01T10:00:00Z"), "{action}");
    assert!(action.contains("stacked on `feature`"), "{action}");
    assert!(action.contains("retargeted to `main`"), "{action}");
    assert_eq!(
        calls.lines().count(),
        2,
        "a merged pull request costs the view and the settings read only:\n{calls}"
    );
}

#[cfg(unix)]
#[test]
fn a_stacked_pull_request_waits_for_its_open_parent() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let parent = r#"  *"pr list --repo owner/repo --head feat/parent"*) printf '%s' '[{"number":7}]' ;;
"#;
    let body = script(
        &log,
        &pr_row("OPEN", "feat/parent", "CLEAN", 3, &green_rollup()),
        "",
        parent,
    );
    let (snapshot, _) = read_snapshot(&temp, &body);
    assert_eq!(
        snapshot["blocked_on"], "stacked_on:feat/parent",
        "{snapshot:#}"
    );
    assert_eq!(snapshot["actionable"], false);
    assert_eq!(snapshot["next_action"]["actor"], "steward");
    let action = snapshot["next_action"]["action"]
        .as_str()
        .expect("action text");
    assert!(
        action.contains("shipyard wait pr 7 --state merged"),
        "{action}"
    );

    // The parent gone (merged, branch kept): retarget now.
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let no_parent = r#"  *"pr list --repo owner/repo --head feat/parent"*) printf '%s' '[]' ;;
"#;
    let body = script(
        &log,
        &pr_row("OPEN", "feat/parent", "CLEAN", 3, &green_rollup()),
        "",
        no_parent,
    );
    let (snapshot, _) = read_snapshot(&temp, &body);
    assert_eq!(snapshot["actionable"], true);
    assert_eq!(snapshot["next_action"]["actor"], "author");
    assert!(
        snapshot["next_action"]["action"]
            .as_str()
            .is_some_and(|action| action.contains("retarget this pull request to `main` now")),
        "{snapshot:#}"
    );
}

#[cfg(unix)]
#[test]
fn an_empty_diff_is_the_authors_move_whatever_the_checks_say() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let body = script(
        &log,
        &pr_row("OPEN", "main", "CLEAN", 0, &green_rollup()),
        "",
        "",
    );
    let (snapshot, calls) = read_snapshot(&temp, &body);
    assert_eq!(snapshot["blocked_on"], "empty_diff", "{snapshot:#}");
    assert_eq!(snapshot["actionable"], true);
    assert_eq!(snapshot["next_action"]["actor"], "author");
    assert!(
        snapshot["next_action"]["action"]
            .as_str()
            .is_some_and(|action| action.contains("0 changed files")),
        "{snapshot:#}"
    );
    assert!(
        !calls.contains("isInMergeQueue"),
        "an empty diff is answered before the admission read:\n{calls}"
    );
}

#[cfg(unix)]
#[test]
fn a_missing_pull_request_is_an_error_the_wait_reports_as_absent() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r"echo 'GraphQL: Could not resolve to a PullRequest with the number of 42. (repository.pullRequest)' >&2; exit 1",
    );
    let error =
        pr_next_action_snapshot(&actions, temp.path(), "owner/repo", 42).expect_err("absent PR");
    assert!(error.starts_with("pull request failed"), "{error}");
    let fetched =
        crate::app::wait_cmd::fetch_pr_actionable_snapshot(&actions, temp.path(), "owner/repo", 42)
            .expect("absent is Ok(None)");
    assert!(fetched.is_none());
}
