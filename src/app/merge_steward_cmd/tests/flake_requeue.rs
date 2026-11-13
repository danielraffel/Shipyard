//! The flake class through the real enqueue path: an approved head the
//! queue ejected for a test that also ejected another pull request's head is
//! re-enqueued once, and never when a defect-fix removal names it.
use super::*;
use crate::app::merge_steward_cmd::defect_fix_removals::{Reading, Removal};
use crate::app::merge_steward_cmd::flake_requeue::{Sighting, Signature, receipt_key};
use crate::app::merge_steward_cmd::pr_mutations::mutate_pr;
use crate::merge_steward::EjectionGuards;
use crate::merge_steward::flake_requeue::{FlakeMode, FlakePolicy};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const GROUP: &str = "c0ffee0000000000000000000000000000000001";
const BASE_TIP: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const MIDI: &str = "CoreMIDI virtual port delivers SysEx";
const CONFIG: &str = r"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = false' '[queue.environment_requeue]' 'enabled = true'";
/// A base that requires an exact-head approval from the review App.
const APPROVAL_REQUIRED: &str = r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["shipyard-review[bot]"]' '[queue.environment_requeue]' 'enabled = true'"#;
/// A HOLD on the ejected head.
const HOLD: &str = r#"{"id":12345,"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-31T22:00:00Z","body":"verdict: HOLD head:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","performed_via_github_app_id":99}"#;

/// What the fake `gh` serves beyond the fixed answers.
#[derive(Clone, Copy)]
struct Serve<'a> {
    config: &'a str,
    comments: &'a str,
    /// Serve the failing job's log; `false` makes it unreadable.
    log: bool,
}

const PLAIN: Serve<'static> = Serve {
    config: CONFIG,
    comments: "",
    log: true,
};

/// The merge group's `macos` job log: ctest names one failing test.
fn job_log() -> String {
    [
        "2026-10-31T21:05:00.0000000Z ##[group]Run ctest --output-on-failure",
        "2026-10-31T21:05:00.0000000Z ctest --output-on-failure",
        "2026-10-31T21:05:00.0000000Z ##[endgroup]",
        "2026-10-31T21:06:00.0000000Z 99% tests passed, 1 tests failed out of 24365",
        "2026-10-31T21:06:00.0000000Z The following tests FAILED:",
        &format!("2026-10-31T21:06:00.0000000Z \t13700 - {MIDI} (Failed)"),
        "2026-10-31T21:06:00.0000000Z Errors while running CTest",
        "2026-10-31T21:06:00.0000000Z ##[error]Process completed with exit code 8.",
    ]
    .join("\n")
        + "\n"
}

/// A fake `gh` for PR 42, whose unchanged head the queue ejected once for
/// `failed_checks` from [`GROUP`], whose `macos` job failed on [`MIDI`].
fn script(temp: &tempfile::TempDir, serve: Serve<'_>) -> String {
    let log = temp.path().join("calls");
    let log_path = temp.path().join("job.log");
    fs::write(&log_path, job_log()).expect("job log");
    r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '__LOG__'
case "$*" in
  *"contents/.shipyard/config.toml"*) __CONFIG__ ;;
  *"isInMergeQueue"*) printf '%s' '{"data":{"repository":{"pullRequest":{"number":42,"state":"OPEN","headRefOid":"__HEAD__","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[{"__typename":"PullRequestCommit","commit":{"oid":"__HEAD__"}},{"__typename":"AddedToMergeQueueEvent","createdAt":"2026-10-31T21:00:00Z"},{"__typename":"RemovedFromMergeQueueEvent","createdAt":"2026-10-31T21:10:00Z","reason":"failed_checks","beforeCommit":{"oid":"__GROUP__","parents":{"nodes":[{"oid":"__BASE_TIP__"},{"oid":"__HEAD__"}]}}}]}}}}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  "pr view "*)
    printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"__HEAD__","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}]}' ;;
  *"pulls/42/reviews"*) printf '' ;;
  *"issues/42/comments"*) printf '%s\n' '__COMMENTS__' ;;
  *"commits/__HEAD__"*) printf '%s' '2026-10-31T00:00:00Z' ;;
  *"repos/owner/repo/pulls/42") printf '%s' '{"base":{"ref":"main"}}' ;;
  *"rules/branches/main"*) printf '%s' '[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"macos"}]}}]' ;;
  *"protection/required_status_checks"*) echo 'HTTP 404: Not Found' >&2; exit 1 ;;
  *"commits/__GROUP__/check-runs"*) printf '%s' '{"total_count":1,"check_runs":[{"id":7,"name":"macos","status":"completed","conclusion":"failure","app":{"slug":"github-actions"}}]}' ;;
  *"commits/__GROUP__/status"*) printf '%s' '{"statuses":[]}' ;;
  *"actions/jobs/7/logs"*) __JOB_LOG_ANSWER__ ;;
  *"actions/jobs/7"*) printf '%s' '{"id":7,"status":"completed","conclusion":"failure","steps":[{"name":"Test","conclusion":"failure","started_at":"2026-10-31T21:05:00Z"}]}' ;;
  *"check-runs/7/annotations"*) printf '%s' '[]' ;;
  *"stackConfig"*) printf '%s' '{"data":{"repository":{"stackConfig":null,"pullRequest":{"headRefOid":"__HEAD__","stack":null,"stackEntry":null}}}}' ;;
  *"stackEntry"*) printf '%s' '{"data":{"repository":{"pullRequest":{"stack":null,"stackEntry":null}}}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"position":1}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#
    .replace("__LOG__", &log.display().to_string())
    .replace(
        "__JOB_LOG_ANSWER__",
        &if serve.log {
            format!("cat '{}'", log_path.display())
        } else {
            "echo 'HTTP 502: Bad Gateway' >&2; exit 1".to_owned()
        },
    )
    .replace("__CONFIG__", serve.config)
    .replace("__COMMENTS__", serve.comments)
    .replace("__HEAD__", HEAD)
    .replace("__GROUP__", GROUP)
    .replace("__BASE_TIP__", BASE_TIP)
}

/// PR 43's head, ejected an hour earlier by another merge group for the
/// same test.
fn other_pr_sighting() -> Sighting {
    Sighting {
        repo: "owner/repo".to_owned(),
        pr: 43,
        head: "dddddddddddddddddddddddddddddddddddddddd".to_owned(),
        signature: Signature {
            text: format!("macos: {MIDI}"),
            checks: vec!["macos".to_owned()],
            tests: vec![MIDI.to_owned()],
        },
        merge_group: "c0ffee0000000000000000000000000000000002".to_owned(),
        at: "2026-10-31T20:10:00Z".to_owned(),
        seen_epoch: chrono::Utc::now().timestamp(),
    }
}

struct Pass {
    mutation: Option<String>,
    error: Option<String>,
    calls: String,
}

fn pass(
    temp: &tempfile::TempDir,
    ledger: &mut StewardLedger,
    defect_fix: Option<&Reading>,
    serve: Serve<'_>,
) -> Pass {
    let log = temp.path().join("calls");
    let _ = fs::remove_file(&log);
    let actions = fake_gh(temp, &script(temp, serve));
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let control = mutation_control(temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    context.ejection_guards = EjectionGuards {
        flake_requeue: FlakePolicy {
            mode: FlakeMode::Apply,
            window_hours: 24,
        },
        ..EjectionGuards::for_base(true)
    };
    context.defect_fix = defect_fix;
    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        ledger,
    );
    Pass {
        mutation,
        error,
        calls: fs::read_to_string(log).unwrap_or_default(),
    }
}

#[cfg(unix)]
#[test]
fn a_flake_ejection_corroborated_by_another_pr_is_requeued_once() {
    let temp = tempfile::tempdir().expect("temp");
    let mut ledger = StewardLedger::default();
    ledger.flake_requeue.sightings.push(other_pr_sighting());
    let first = pass(&temp, &mut ledger, None, PLAIN);
    assert!(first.error.is_none(), "{:?}\n{}", first.error, first.calls);
    assert!(
        first
            .mutation
            .as_deref()
            .is_some_and(|mutation| mutation.starts_with("enqueued")),
        "{:?}\n{}",
        first.mutation,
        first.calls
    );
    let key = receipt_key("owner/repo", 42, HEAD);
    assert!(ledger.flake_requeue.requeued.contains_key(&key));
    // The receipt was saved before the enqueue.
    let saved = fs::read_to_string(temp.path().join("ledger.json")).expect("ledger saved");
    assert!(saved.contains(&key), "{saved}");
    // The same head ejected again: no second flake requeue.
    let second = pass(&temp, &mut ledger, None, PLAIN);
    assert!(
        !second.calls.contains("enqueuePullRequest"),
        "{}",
        second.calls
    );
    assert_eq!(
        second.mutation.as_deref(),
        Some("skipped_ejected_same_head:failed_checks:needs_new_head"),
        "{:?}",
        second.error
    );
}

#[cfg(unix)]
#[test]
fn a_lone_flake_ejection_keeps_the_refusal() {
    let temp = tempfile::tempdir().expect("temp");
    let mut ledger = StewardLedger::default();
    let pass = pass(&temp, &mut ledger, None, PLAIN);
    assert!(!pass.calls.contains("enqueuePullRequest"), "{}", pass.calls);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("skipped_ejected_same_head:failed_checks:needs_new_head"),
        "{:?}\n{}",
        pass.error,
        pass.calls
    );
    // It was read and remembered, so a second pull request can corroborate it.
    assert_eq!(ledger.flake_requeue.sightings.len(), 1);
    assert_eq!(
        ledger.flake_requeue.sightings[0].signature.text,
        format!("macos: {MIDI}")
    );
}

#[cfg(unix)]
#[test]
fn a_defect_fix_removed_flake_head_is_never_requeued() {
    let temp = tempfile::tempdir().expect("temp");
    let mut ledger = StewardLedger::default();
    ledger.flake_requeue.sightings.push(other_pr_sighting());
    let reading = Reading {
        removals: vec![Removal {
            repo: "owner/repo".to_owned(),
            pr: 42,
            head: HEAD.to_owned(),
            reason: "defect-fix".to_owned(),
            at: "2026-10-31T21:20:00Z".to_owned(),
            note: None,
            host: None,
        }],
        ..Reading::default()
    };
    let pass = pass(&temp, &mut ledger, Some(&reading), PLAIN);
    assert!(!pass.calls.contains("enqueuePullRequest"), "{}", pass.calls);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("defect_fix_removed:defect-fix")
    );
    assert!(ledger.flake_requeue.requeued.is_empty());
}

/// The flake class never stands in for approval: the requeue goes through
/// the steward's live exact-head approval read, and a HOLD, or no approval
/// at all, stops it before the flake class is consulted.
#[cfg(unix)]
#[test]
fn a_flake_requeue_needs_the_live_exact_head_approval() {
    for comments in [HOLD, ""] {
        let temp = tempfile::tempdir().expect("temp");
        let mut ledger = StewardLedger::default();
        ledger.flake_requeue.sightings.push(other_pr_sighting());
        let pass = pass(
            &temp,
            &mut ledger,
            None,
            Serve {
                config: APPROVAL_REQUIRED,
                comments,
                log: true,
            },
        );
        assert!(!pass.calls.contains("enqueuePullRequest"), "{}", pass.calls);
        assert_eq!(
            pass.mutation.as_deref(),
            Some("waiting_head_approval"),
            "{:?}\n{}",
            pass.error,
            pass.calls
        );
        assert!(ledger.flake_requeue.requeued.is_empty());
        assert!(ledger.flake_requeue.pass.is_empty());
    }
}

/// A read the signature needs that fails never requeues: no signature, the
/// classifier's refusal stands.
#[cfg(unix)]
#[test]
fn an_unreadable_job_log_never_requeues() {
    let temp = tempfile::tempdir().expect("temp");
    let mut ledger = StewardLedger::default();
    ledger.flake_requeue.sightings.push(other_pr_sighting());
    let pass = pass(
        &temp,
        &mut ledger,
        None,
        Serve {
            log: false,
            ..PLAIN
        },
    );
    assert!(!pass.calls.contains("enqueuePullRequest"), "{}", pass.calls);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("skipped_ejected_same_head:failed_checks:needs_new_head"),
        "{:?}\n{}",
        pass.error,
        pass.calls
    );
    assert!(ledger.flake_requeue.requeued.is_empty());
    // Only the seeded sighting: the unread ejection recorded nothing.
    assert_eq!(ledger.flake_requeue.sightings.len(), 1);
}
