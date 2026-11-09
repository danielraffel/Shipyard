//! The steward's merge-queue ejection guards, through the real enqueue path:
//! the generated-path batch guard, and the retry discipline's refusal of an
//! `invalid_merge_commit` or not-infrastructure same-head ejection. Shapes are
//! the Generous-Corp/pulp incidents of 2026-11-02..09.
use super::*;
use crate::app::merge_steward_cmd::pr_mutations::mutate_pr;
use crate::merge_steward::EjectionGuards;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const GROUP: &str = "c0ffee0000000000000000000000000000000001";
const BASE_TIP: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OPTED_IN: &str = r"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = false' '[queue.environment_requeue]' 'enabled = true'";

fn pulp_guards() -> EjectionGuards {
    EjectionGuards {
        generated_paths: vec![
            "test/ctest_script_inputs.json".to_owned(),
            "test/ctest_script_inputs/**".to_owned(),
            "docs/status/agent-capability-surface.json".to_owned(),
            "tools/scripts/agent_capability_manifest.py".to_owned(),
            "tools/scripts/agent_capability_catalog_*.py".to_owned(),
            "tools/agent-capabilities/contract-history.json".to_owned(),
            "docs/status/consumption-profiles.json".to_owned(),
        ],
        ..EjectionGuards::for_base(true)
    }
}

/// A `files(first:100)` answer for one pull request.
fn files_answer(paths: &[&str]) -> String {
    let nodes = paths
        .iter()
        .map(|path| format!(r#"{{"path":"{path}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"data":{{"repository":{{"pullRequest":{{"files":{{"nodes":[{nodes}],"pageInfo":{{"hasNextPage":false,"endCursor":null}}}}}}}}}}}}"#
    )
}

/// A fake `gh` for one steward pass over PR 42. `removals` is the timeline's
/// queue events after the head commit, `queued` the other queue entries as
/// `(number, files)`, and `group_conclusion` the ejecting merge group's
/// required `macos` check.
fn script(
    log: &Path,
    removals: &str,
    candidate_files: &[&str],
    queued: &[(u64, &[&str])],
    group_conclusion: &str,
) -> String {
    let entries = queued
        .iter()
        .enumerate()
        .map(|(index, (number, _))| {
            format!(
                r#"{{"position":{},"enqueuedAt":"2026-11-06T10:00:00Z","headCommit":{{"oid":"dddddddddddddddddddddddddddddddddddddddd"}},"pullRequest":{{"number":{number}}}}}"#,
                index + 1
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let file_cases = queued
        .iter()
        .fold(String::new(), |mut cases, (number, paths)| {
            use std::fmt::Write as _;
            let _ = writeln!(
                cases,
                "  *\"files(first:100\"*\"number={number}\"*) printf '%s' '{}' ;;",
                files_answer(paths)
            );
            cases
        });
    r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '__LOG__'
case "$*" in
  *"contents/.shipyard/config.toml"*) __CONFIG__ ;;
  *"files(first:100"*"number=42"*) printf '%s' '__CANDIDATE_FILES__' ;;
__FILE_CASES__  *"files(first:100"*) echo "unexpected files read: $*" >&2; exit 2 ;;
  *"isInMergeQueue"*) printf '%s' '{"data":{"repository":{"pullRequest":{"number":42,"state":"OPEN","headRefOid":"__HEAD__","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[{"__typename":"PullRequestCommit","commit":{"oid":"__HEAD__"}}__REMOVALS__]}}}}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[__ENTRIES__],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  "pr view "*)
    printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"__HEAD__","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}]}' ;;
  *"pulls/42/reviews"*) printf '' ;;
  *"issues/42/comments"*) printf '' ;;
  *"commits/__HEAD__/check-runs"*) printf '%s' '{"total_count":0,"check_runs":[]}' ;;
  *"commits/__HEAD__/status"*) printf '%s' '{"statuses":[]}' ;;
  *"commits/__HEAD__"*) printf '%s' '2026-11-06T00:00:00Z' ;;
  *"repos/owner/repo/pulls/42") printf '%s' '{"base":{"ref":"main"}}' ;;
  *"rules/branches/main"*) printf '%s' '[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"macos"}]}}]' ;;
  *"protection/required_status_checks"*) echo 'HTTP 404: Not Found' >&2; exit 1 ;;
  *"commits/__GROUP__/check-runs"*) printf '%s' '{"total_count":1,"check_runs":[{"id":7,"name":"macos","status":"completed","conclusion":"__CONCLUSION__","app":{"slug":"github-actions"}}]}' ;;
  *"commits/__GROUP__/status"*) printf '%s' '{"statuses":[]}' ;;
  *"check-runs/7/annotations"*) printf '%s' '[]' ;;
  *"actions/jobs/7/logs"*) printf '%s\n' '2026-11-06T10:10:00.0000000Z ##[group]Run ctest' '2026-11-06T10:10:00.0000000Z ctest' '2026-11-06T10:10:00.0000000Z ##[endgroup]' '2026-11-06T10:30:00.0000000Z 1/2 Test #4: hosted_editor ....***Failed' '2026-11-06T10:30:01.0000000Z ##[error]Process completed with exit code 8.' ;;
  *"actions/jobs/7"*) printf '%s' '{"id":7,"status":"completed","conclusion":"failure","created_at":"2026-11-06T10:00:00Z","completed_at":"2026-11-06T10:30:01Z","steps":[{"name":"Test","conclusion":"failure","started_at":"2026-11-06T10:10:00Z"}]}' ;;
  *"commits/main/check-runs"*) printf '%s' '{"total_count":1,"check_runs":[{"id":9,"name":"macos","status":"completed","conclusion":"success"}]}' ;;
  *"commits/main/status"*) printf '%s' '{"statuses":[]}' ;;
  *"stackConfig"*) printf '%s' '{"data":{"repository":{"stackConfig":null,"pullRequest":{"headRefOid":"__HEAD__","stack":null,"stackEntry":null}}}}' ;;
  *"stackEntry"*) printf '%s' '{"data":{"repository":{"pullRequest":{"stack":null,"stackEntry":null}}}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"position":1}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#
    .replace("__LOG__", &log.display().to_string())
    .replace("__CONFIG__", OPTED_IN)
    .replace("__CANDIDATE_FILES__", &files_answer(candidate_files))
    .replace("__FILE_CASES__", &file_cases)
    .replace("__REMOVALS__", removals)
    .replace("__ENTRIES__", &entries)
    .replace("__GROUP__", GROUP)
    .replace("__CONCLUSION__", group_conclusion)
    .replace("__HEAD__", HEAD)
}

struct Pass {
    mutation: Option<String>,
    error: Option<String>,
    calls: String,
    blocked_on: String,
}

fn pass(temp: &tempfile::TempDir, body: &str, guards: &EjectionGuards) -> Pass {
    let log = temp.path().join("calls");
    let actions = fake_gh(temp, body);
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let control = mutation_control(temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    context.ejection_guards = guards.clone();
    let mut policy = queue_policy();
    policy.ejection_guards = guards.clone();
    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );
    let approval = context.approval_blockers.borrow();
    let record = approval.get(&42);
    let (stall, _) = crate::app::merge_steward_cmd::blocked_on::classify(
        &crate::app::merge_steward_cmd::blocked_on::RowFacts {
            pr: &pr.fact,
            decision: &StewardDecision::ArmMergeQueue,
            mutation: mutation.as_deref(),
            error: error.as_deref(),
            approval: crate::app::merge_steward_cmd::blocked_on::GateRead::Admits,
            enqueued_at: None,
            ejection: record.and_then(|record| record.ejection.as_ref()),
        },
        &policy,
    );
    Pass {
        mutation: mutation.clone(),
        error: error.clone(),
        calls: fs::read_to_string(log).unwrap_or_default(),
        blocked_on: stall.blocked_on,
    }
}

fn enqueued(pass: &Pass) -> bool {
    pass.calls.contains("enqueuePullRequest")
}

// ── generated-path batch guard ─────────────────────────────────────────────

/// #11719's shape: it and #11720 both regenerate the ctest inputs manifest
/// and the capability surface; #11720 entered the queue first.
#[cfg(unix)]
#[test]
fn a_head_sharing_a_generated_path_with_a_queued_entry_waits_for_it() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(
        &temp.path().join("calls"),
        "",
        &[
            "core/view/src/widgets.cpp",
            "test/ctest_script_inputs.json",
            "docs/status/agent-capability-surface.json",
        ],
        &[
            (11700, &["core/audio/src/reader.cpp"]),
            (
                11720,
                &["core/format/src/vst3.cpp", "test/ctest_script_inputs.json"],
            ),
        ],
        "success",
    );
    let pass = pass(&temp, &body, &pulp_guards());
    assert!(!enqueued(&pass), "{}", pass.calls);
    assert!(pass.error.is_none(), "{:?}", pass.error);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("generated_path_batch:#11720@test/ctest_script_inputs.json"),
        "{}",
        pass.calls
    );
    assert_eq!(pass.blocked_on, "generated_path_batch:#11720");
    // One files read per pull request, and only for the queued entries the
    // guard had to compare.
    assert_eq!(
        pass.calls.matches("files(first:100").count(),
        3,
        "{}",
        pass.calls
    );
}

#[cfg(unix)]
#[test]
fn the_same_head_is_admitted_once_the_queued_entry_has_left() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(
        &temp.path().join("calls"),
        "",
        &["test/ctest_script_inputs.json"],
        &[(11700, &["core/audio/src/reader.cpp", "test/other.json"])],
        "success",
    );
    let pass = pass(&temp, &body, &pulp_guards());
    assert!(
        enqueued(&pass),
        "{:?} {:?}\n{}",
        pass.mutation,
        pass.error,
        pass.calls
    );
    assert_eq!(pass.blocked_on, "admitted");
}

#[cfg(unix)]
#[test]
fn a_head_that_changes_no_generated_path_reads_no_queue_entry() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(
        &temp.path().join("calls"),
        "",
        &["core/view/src/widgets.cpp"],
        &[(11720, &["test/ctest_script_inputs.json"])],
        "success",
    );
    let pass = pass(&temp, &body, &pulp_guards());
    assert!(enqueued(&pass), "{:?}\n{}", pass.mutation, pass.calls);
    assert_eq!(
        pass.calls.matches("files(first:100").count(),
        1,
        "{}",
        pass.calls
    );
}

#[cfg(unix)]
#[test]
fn the_batch_guard_is_off_when_its_switch_is() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(
        &temp.path().join("calls"),
        "",
        &["test/ctest_script_inputs.json"],
        &[(11720, &["test/ctest_script_inputs.json"])],
        "success",
    );
    let guards = EjectionGuards {
        batch_guard: false,
        ..pulp_guards()
    };
    let pass = pass(&temp, &body, &guards);
    assert!(enqueued(&pass), "{:?}\n{}", pass.mutation, pass.calls);
    assert!(!pass.calls.contains("files(first:100"), "{}", pass.calls);
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_list_never_admits() {
    let temp = tempfile::tempdir().expect("temp");
    // #11720's files query is not answered by this fixture.
    let body = script(
        &temp.path().join("calls"),
        "",
        &["test/ctest_script_inputs.json"],
        &[],
        "success",
    )
    .replace(
        r#"  *"query=query("*"mergeQueue"*)"#,
        r#"  *"query=query("*"mergeQueue(branch"*) printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":1,"enqueuedAt":"2026-11-06T10:00:00Z","headCommit":{"oid":"dddddddddddddddddddddddddddddddddddddddd"},"pullRequest":{"number":11720}}],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  *"query=query("*"mergeQueue"*)"#,
    );
    let pass = pass(&temp, &body, &pulp_guards());
    assert!(!enqueued(&pass), "{}", pass.calls);
    assert!(
        pass.error
            .as_deref()
            .is_some_and(|error| error.contains("changed files of #11720 unreadable")),
        "{:?}\n{}",
        pass.error,
        pass.calls
    );
}

// ── retry discipline ───────────────────────────────────────────────────────

fn removal(reason: &str, minute: u32, group: bool) -> String {
    let before = if group {
        format!(
            r#"{{"oid":"{GROUP}","parents":{{"nodes":[{{"oid":"{BASE_TIP}"}},{{"oid":"{HEAD}"}}]}}}}"#
        )
    } else {
        "null".to_owned()
    };
    format!(
        r#",{{"__typename":"AddedToMergeQueueEvent","createdAt":"2026-11-07T02:{minute:02}:00Z"}},{{"__typename":"RemovedFromMergeQueueEvent","createdAt":"2026-11-07T02:{minute:02}:15Z","reason":"{reason}","beforeCommit":{before}}}"#
    )
}

/// #11698's timeline: `failed_checks`, then the same head re-enqueued and
/// removed at once for `invalid_merge_commit`, which the steward re-enqueued
/// twice more within 21 minutes.
fn pr_11698_timeline() -> String {
    [
        removal("failed_checks", 25, true),
        removal("invalid_merge_commit", 33, false),
        removal("invalid_merge_commit", 44, false),
    ]
    .concat()
}

#[cfg(unix)]
#[test]
fn an_invalid_merge_commit_head_is_not_re_enqueued_and_needs_a_new_head() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(
        &temp.path().join("calls"),
        &pr_11698_timeline(),
        &["web/CMakeLists.txt"],
        &[],
        "success",
    );
    let pass = pass(&temp, &body, &EjectionGuards::for_base(true));
    assert!(!enqueued(&pass), "{}", pass.calls);
    assert!(pass.error.is_none(), "{:?}", pass.error);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("skipped_ejected_same_head:invalid_merge_commit:needs_new_head")
    );
    assert_eq!(
        pass.blocked_on,
        "ejected_same_head:invalid_merge_commit:needs_new_head"
    );
}

/// The control: without the discipline the same timeline re-enqueues, which
/// is the loop #11698 went round.
#[cfg(unix)]
#[test]
fn without_the_discipline_an_invalid_merge_commit_head_is_re_enqueued() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(
        &temp.path().join("calls"),
        &pr_11698_timeline(),
        &["web/CMakeLists.txt"],
        &[],
        "success",
    );
    let pass = pass(&temp, &body, &EjectionGuards::default());
    assert!(enqueued(&pass), "{:?}\n{}", pass.mutation, pass.calls);
}

/// #11471's shape: a merge-group-only test failure that passed on the head.
/// The old not-head-caused allowance retried it on the same head, and it
/// failed again the same way.
fn passed_on_head_body(log: &Path) -> String {
    script(
        log,
        &removal("failed_checks", 25, true),
        &["core/format/src/hosted_editor.cpp"],
        &[],
        "failure",
    )
    .replace(
        &format!(r#"  *"commits/{HEAD}/check-runs"*) printf '%s' '{{"total_count":0,"check_runs":[]}}' ;;"#),
        &format!(
            r#"  *"commits/{HEAD}/check-runs"*) printf '%s' '{{"total_count":1,"check_runs":[{{"id":8,"name":"macos","status":"completed","conclusion":"success"}}]}}' ;;"#
        ),
    )
}

#[cfg(unix)]
#[test]
fn a_test_failure_that_passed_on_the_head_is_not_retried_under_the_discipline() {
    let temp = tempfile::tempdir().expect("temp");
    let body = passed_on_head_body(&temp.path().join("calls"));
    let strict = pass(&temp, &body, &EjectionGuards::for_base(true));
    assert!(!enqueued(&strict), "{}", strict.calls);
    assert!(strict.error.is_none(), "{:?}", strict.error);
    assert_eq!(
        strict.mutation.as_deref(),
        Some("skipped_ejected_same_head:failed_checks:needs_new_head")
    );
    assert_eq!(
        strict.blocked_on,
        "ejected_same_head:failed_checks:needs_new_head"
    );
    // Control: the old allowance, same answers, re-enqueues it.
    let temp = tempfile::tempdir().expect("temp");
    let body = passed_on_head_body(&temp.path().join("calls"));
    let lenient = pass(&temp, &body, &EjectionGuards::default());
    assert!(
        enqueued(&lenient),
        "{:?}\n{}",
        lenient.mutation,
        lenient.calls
    );
}

/// A runner timeout is infrastructure: the discipline still spends the one
/// retry on it.
#[cfg(unix)]
#[test]
fn a_runner_timeout_still_gets_its_one_retry_under_the_discipline() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(
        &temp.path().join("calls"),
        &removal("failed_checks", 25, true),
        &["core/a.cpp"],
        &[],
        "timed_out",
    );
    let pass = pass(&temp, &body, &EjectionGuards::for_base(true));
    assert!(
        enqueued(&pass),
        "{:?} {:?}\n{}",
        pass.mutation,
        pass.error,
        pass.calls
    );
}

#[cfg(unix)]
#[test]
fn an_empty_queue_reads_no_file_list() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(
        &temp.path().join("calls"),
        "",
        &["test/ctest_script_inputs.json"],
        &[],
        "success",
    );
    let pass = pass(&temp, &body, &pulp_guards());
    assert!(enqueued(&pass), "{:?}\n{}", pass.mutation, pass.calls);
    assert!(!pass.calls.contains("files(first:100"), "{}", pass.calls);
}

/// The pass-start config read failed (a transient 5xx), so the pass holds no
/// generated paths. The guard re-reads the config live rather than admitting
/// unguarded; here the re-read succeeds and finds the collision.
#[cfg(unix)]
#[test]
fn a_failed_config_read_is_re_read_before_admitting() {
    let temp = tempfile::tempdir().expect("temp");
    let body = script(
        &temp.path().join("calls"),
        "",
        &["test/ctest_script_inputs.json"],
        &[(11720, &["test/ctest_script_inputs.json"])],
        "success",
    )
    .replace(
        OPTED_IN,
        r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = false' 'generated_paths = ["test/ctest_script_inputs.json"]'"#,
    );
    let degraded = EjectionGuards {
        read_error: Some("HTTP 502: Bad Gateway".to_owned()),
        ..EjectionGuards::for_base(true)
    };
    let pass = pass(&temp, &body, &degraded);
    assert!(!enqueued(&pass), "{}", pass.calls);
    assert_eq!(
        pass.blocked_on, "generated_path_batch:#11720",
        "{:?}",
        pass.error
    );
}

/// #11584's shape through the steward: only a listed flake failed in the
/// merge group, and the check passed on the head, so the base's
/// `known_flaky_tests` earns the one retry under the discipline.
#[cfg(unix)]
#[test]
fn a_known_flake_listed_in_the_base_config_is_re_enqueued_once() {
    let body = |log: &Path| {
        passed_on_head_body(log).replace(
            "'2026-11-06T10:30:00.0000000Z 1/2 Test #4: hosted_editor ....***Failed'",
            "'2026-11-06T10:30:00.0000000Z The following tests FAILED:' '2026-11-06T10:30:00.0000000Z \t13716 - ui-script-plugin-editor-au (Failed)'",
        )
    };
    let listed = EjectionGuards {
        known_flaky_tests: vec!["ui-script-plugin-editor-au".to_owned()],
        ..EjectionGuards::for_base(true)
    };
    let temp = tempfile::tempdir().expect("temp");
    let pass_listed = pass(&temp, &body(&temp.path().join("calls")), &listed);
    assert!(
        enqueued(&pass_listed),
        "{:?} {:?}\n{}",
        pass_listed.mutation,
        pass_listed.error,
        pass_listed.calls
    );
    // Control: the same ejection with nothing listed needs a new head.
    let temp = tempfile::tempdir().expect("temp");
    let unlisted = pass(
        &temp,
        &body(&temp.path().join("calls")),
        &EjectionGuards::for_base(true),
    );
    assert!(!enqueued(&unlisted), "{}", unlisted.calls);
    assert_eq!(
        unlisted.blocked_on,
        "ejected_same_head:failed_checks:needs_new_head"
    );
}
