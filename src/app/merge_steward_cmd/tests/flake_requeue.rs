//! The flake class through the real enqueue path, on a base that requires
//! an exact-head approval: an approved head the queue ejected for a test
//! that also ejected another pull request's head is re-enqueued once, and
//! never when the approval, the head, the base reading or the receipt says
//! otherwise.
use super::*;
use crate::app::merge_steward_cmd::base_poison::RepoPoison;
use crate::app::merge_steward_cmd::defect_fix_removals::{Reading, Removal};
use crate::app::merge_steward_cmd::flake_requeue::{
    Receipt, ReceiptState, Sighting, Signature, green_base, receipt_key,
};
use crate::app::merge_steward_cmd::pr_mutations::mutate_pr;
use crate::merge_steward::EjectionGuards;
use crate::merge_steward::flake_requeue::{FlakeMode, FlakePolicy};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OLD_HEAD: &str = "9999999999999999999999999999999999999999";
const MOVED: &str = "cccccccccccccccccccccccccccccccccccccccc";
const GROUP: &str = "c0ffee0000000000000000000000000000000001";
const BASE_TIP: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const MIDI: &str = "CoreMIDI virtual port delivers SysEx";
const GENERATED: &str = "test/ctest_script_inputs.json";
const GROUP_2: &str = "c0ffee0000000000000000000000000000000009";
/// The base tip's newest completed `macos` run passed.
const TIP_GREEN: &str = r#"{"total_count":1,"check_runs":[{"id":31,"name":"macos","status":"completed","conclusion":"success"}]}"#;
/// The base tip has no completed `macos` run.
const TIP_EMPTY: &str = r#"{"total_count":0,"check_runs":[]}"#;
/// The base tip's only `macos` run is still running.
const TIP_PENDING: &str = r#"{"total_count":1,"check_runs":[{"id":32,"name":"macos","status":"in_progress","conclusion":null}]}"#;
/// The base requires an exact-head approval from the review App.
const CONFIG: &str = r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["shipyard-review[bot]"]' '[queue.environment_requeue]' 'enabled = true'"#;

fn verdict_comment(id: u64, at: &str, verdict: &str, head: &str) -> String {
    format!(
        r#"{{"id":{id},"login":"shipyard-review[bot]","type":"Bot","at":"{at}","body":"verdict: {verdict} head:{head}","performed_via_github_app_id":99}}"#
    )
}

fn approve() -> String {
    verdict_comment(12001, "2026-10-31T20:00:00Z", "APPROVE", HEAD)
}

/// The live pull request as `pr view` answers it.
fn live_pr(head: &str, draft: bool, labels: &str) -> String {
    format!(
        r#"{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":{draft},"baseRefName":"main","headRefOid":"{head}","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{labels}],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}"#
    )
}

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

/// A `files(first:100)` answer.
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

/// What the fake `gh` serves for one pass.
struct Serve {
    comments: String,
    /// Serve the failing job's log; `false` makes it unreadable.
    log: bool,
    /// The live `pr view` answer.
    pr_view: String,
    /// Another queue entry (#77) that changes the generated path PR 42 also
    /// changes: the generated-path batch guard defers the enqueue.
    batch_mate: bool,
    /// The base tip's `macos` check runs (a check-runs answer).
    tip: String,
    /// The head's `failed_checks` removals: `(at, merge group)`.
    removals: Vec<(&'static str, &'static str)>,
}

impl Serve {
    fn approved() -> Self {
        Self {
            comments: approve(),
            log: true,
            pr_view: live_pr(HEAD, false, ""),
            batch_mate: false,
            tip: TIP_GREEN.to_owned(),
            removals: vec![("2026-10-31T21:10:00Z", GROUP)],
        }
    }
}

/// A fake `gh` for PR 42, whose unchanged head the queue ejected once for
/// `failed_checks` from [`GROUP`], whose `macos` job failed on [`MIDI`].
fn script(temp: &tempfile::TempDir, serve: &Serve) -> String {
    let log = temp.path().join("calls");
    let log_path = temp.path().join("job.log");
    fs::write(&log_path, job_log()).expect("job log");
    let entries = if serve.batch_mate {
        r#"{"position":1,"enqueuedAt":"2026-11-06T10:00:00Z","headCommit":{"oid":"dddddddddddddddddddddddddddddddddddddddd"},"pullRequest":{"number":77}}"#
    } else {
        ""
    };
    r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '__LOG__'
case "$*" in
  *"contents/.shipyard/config.toml"*) __CONFIG__ ;;
  *"files(first:100"*"number=42"*) printf '%s' '__FILES__' ;;
  *"files(first:100"*"number=77"*) printf '%s' '__FILES__' ;;
  *"isInMergeQueue"*) printf '%s' '{"data":{"repository":{"pullRequest":{"number":42,"state":"OPEN","headRefOid":"__HEAD__","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[{"__typename":"PullRequestCommit","commit":{"oid":"__HEAD__"}},__REMOVALS__]}}}}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[__ENTRIES__],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  "pr view "*) printf '%s' '__PR_VIEW__' ;;
  *"pulls/42/reviews"*) printf '' ;;
  *"issues/42/comments"*) printf '%s\n' '__COMMENTS__' ;;
  *"commits/__HEAD__"*) printf '%s' '2026-10-31T00:00:00Z' ;;
  *"repos/owner/repo/pulls/42") printf '%s' '{"base":{"ref":"main"}}' ;;
  *"rules/branches/main"*) printf '%s' '[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"macos"}]}}]' ;;
  *"protection/required_status_checks"*) echo 'HTTP 404: Not Found' >&2; exit 1 ;;
  *"commits/main/check-runs"*) printf '%s' '__TIP__' ;;
  *"commits/c0ffee"*"/check-runs"*) printf '%s' '{"total_count":1,"check_runs":[{"id":7,"name":"macos","status":"completed","conclusion":"failure","app":{"slug":"github-actions"}}]}' ;;
  *"commits/c0ffee"*"/status"*) printf '%s' '{"statuses":[]}' ;;
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
    .replace("__CONFIG__", CONFIG)
    .replace("__COMMENTS__", &serve.comments)
    .replace("__PR_VIEW__", &serve.pr_view)
    .replace("__ENTRIES__", entries)
    .replace("__TIP__", &serve.tip)
    .replace(
        "__REMOVALS__",
        &serve
            .removals
            .iter()
            .map(|(at, group)| {
                format!(
                    r#"{{"__typename":"AddedToMergeQueueEvent","createdAt":"{at}"}},{{"__typename":"RemovedFromMergeQueueEvent","createdAt":"{at}","reason":"failed_checks","beforeCommit":{{"oid":"{group}","parents":{{"nodes":[{{"oid":"{BASE_TIP}"}},{{"oid":"{HEAD}"}}]}}}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(","),
    )
    .replace("__FILES__", &files_answer(&[GENERATED]))
    .replace("__HEAD__", HEAD)
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

fn seeded() -> StewardLedger {
    let mut ledger = StewardLedger::default();
    ledger.flake_requeue.sightings.push(other_pr_sighting());
    ledger
}

struct Pass {
    mutation: Option<String>,
    error: Option<String>,
    calls: String,
}

impl Pass {
    fn enqueued(&self) -> bool {
        self.calls.contains("enqueuePullRequest")
    }
}

/// One steward pass over PR 42 with the flake class on `apply`.
fn pass(
    temp: &tempfile::TempDir,
    ledger: &mut StewardLedger,
    serve: &Serve,
    defect_fix: Option<&Reading>,
    poison: Option<&RepoPoison>,
) -> Pass {
    let log = temp.path().join("calls");
    let _ = fs::remove_file(&log);
    let actions = fake_gh(temp, &script(temp, serve));
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let control = mutation_control(temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let guards = EjectionGuards {
        flake_requeue: FlakePolicy {
            mode: FlakeMode::Apply,
            window_hours: 24,
            max_requeues: 3,
        },
        generated_paths: vec![GENERATED.to_owned()],
        ..EjectionGuards::for_base(true)
    };
    let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    context.ejection_guards = guards.clone();
    context.defect_fix = defect_fix;
    context.base_poison = poison;
    let mut policy = queue_policy();
    policy.ejection_guards = guards;
    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &policy,
        &StewardDecision::ArmMergeQueue,
        ledger,
    );
    Pass {
        mutation,
        error,
        calls: fs::read_to_string(log).unwrap_or_default(),
    }
}

const REFUSED: &str = "skipped_ejected_same_head:failed_checks:needs_new_head";

fn key() -> String {
    receipt_key("owner/repo", 42, HEAD)
}

#[cfg(unix)]
#[test]
fn an_approved_flake_ejection_corroborated_by_another_pr_is_requeued_once() {
    let temp = tempfile::tempdir().expect("temp");
    let green = green_base(&["macos"]);
    let mut ledger = seeded();
    let first = pass(&temp, &mut ledger, &Serve::approved(), None, Some(&green));
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
    let receipt = &ledger.flake_requeue.requeued[&key()];
    assert_eq!(receipt.state, ReceiptState::Done);
    // The receipt reached disk before the enqueue, and its `done` after.
    let saved = fs::read_to_string(temp.path().join("ledger.json")).expect("ledger saved");
    assert!(saved.contains(&key()), "{saved}");
    // The same head ejected again: no second flake requeue.
    let second = pass(&temp, &mut ledger, &Serve::approved(), None, Some(&green));
    assert!(!second.enqueued(), "{}", second.calls);
    assert_eq!(
        second.mutation.as_deref(),
        Some(REFUSED),
        "{:?}",
        second.error
    );
}

#[cfg(unix)]
#[test]
fn a_lone_flake_ejection_keeps_the_refusal() {
    let temp = tempfile::tempdir().expect("temp");
    let green = green_base(&["macos"]);
    let mut ledger = StewardLedger::default();
    let pass = pass(&temp, &mut ledger, &Serve::approved(), None, Some(&green));
    assert!(!pass.enqueued(), "{}", pass.calls);
    assert_eq!(
        pass.mutation.as_deref(),
        Some(REFUSED),
        "{:?}\n{}",
        pass.error,
        pass.calls
    );
    assert_eq!(ledger.flake_requeue.sightings.len(), 1);
    assert_eq!(
        ledger.flake_requeue.sightings[0].signature.text,
        format!("macos: {MIDI}")
    );
}

/// The flake class never stands in for approval or for a live head: each
/// case stops before any flake re-enqueue and writes no receipt.
#[cfg(unix)]
#[test]
fn no_requeue_without_the_live_approved_unchanged_ready_head() {
    let green = green_base(&["macos"]);
    let held = format!(
        "{}\n{}",
        approve(),
        verdict_comment(12002, "2026-10-31T22:00:00Z", "HOLD", HEAD)
    );
    let cases = [
        (
            "withdrawn approval (a later HOLD)",
            held,
            live_pr(HEAD, false, ""),
        ),
        (
            "no approval at all",
            String::new(),
            live_pr(HEAD, false, ""),
        ),
        (
            "approval of an older head",
            verdict_comment(12003, "2026-10-30T20:00:00Z", "APPROVE", OLD_HEAD),
            live_pr(HEAD, false, ""),
        ),
        ("moved head", approve(), live_pr(MOVED, false, "")),
        ("draft", approve(), live_pr(HEAD, true, "")),
        (
            "owner-decision label",
            approve(),
            live_pr(HEAD, false, r#"{"name":"owner-decision"}"#),
        ),
    ];
    for (case, comments, pr_view) in cases {
        let temp = tempfile::tempdir().expect("temp");
        let mut ledger = seeded();
        let serve = Serve {
            comments,
            pr_view,
            ..Serve::approved()
        };
        let pass = pass(&temp, &mut ledger, &serve, None, Some(&green));
        assert!(!pass.enqueued(), "{case}: {}", pass.calls);
        assert!(
            !pass
                .mutation
                .as_deref()
                .is_some_and(|mutation| mutation.contains("enqueued")),
            "{case}: {:?}",
            pass.mutation
        );
        assert!(ledger.flake_requeue.requeued.is_empty(), "{case}");
    }
}

/// The base reading through the real path: unread, partial or red refuses.
#[cfg(unix)]
#[test]
fn an_unread_partial_or_red_base_never_requeues() {
    let partial = RepoPoison {
        fix_read_error: Some("fixes unread".to_owned()),
        ..green_base(&["macos"])
    };
    let red = RepoPoison {
        health: crate::base_health::merge_group::MergeGroupHealth::Poisoned {
            contexts: vec![crate::base_health::merge_group::PoisonedContext {
                context: "macos".to_owned(),
                first_error: None,
                groups: Vec::new(),
                latest_failure_at: None,
                check_id: None,
                same_first_error: None,
            }],
        },
        tip: std::collections::BTreeMap::from([(
            "macos".to_owned(),
            crate::base_health::fix_first::TipReading {
                sha: None,
                red: true,
                red_since: None,
                newest_at: None,
                error: None,
            },
        )]),
        ..green_base(&[])
    };
    for (poison, outcome) in [
        (None, "base_unknown"),
        (Some(&partial), "base_unknown"),
        (Some(&red), "base_red"),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let mut ledger = seeded();
        let pass = pass(&temp, &mut ledger, &Serve::approved(), None, poison);
        assert!(!pass.enqueued(), "{outcome}: {}", pass.calls);
        assert!(ledger.flake_requeue.requeued.is_empty());
        assert_eq!(
            ledger
                .flake_requeue
                .pass
                .last()
                .map(|decision| decision.outcome),
            Some(outcome)
        );
    }
}

/// A deferral after the receipt was saved strands nothing: the receipt
/// stays `pending`, waits a tick, and is retried once.
#[cfg(unix)]
#[test]
fn a_deferred_requeue_stays_pending_and_is_retried_once() {
    let temp = tempfile::tempdir().expect("temp");
    let green = green_base(&["macos"]);
    let mut ledger = seeded();
    let deferred = Serve {
        batch_mate: true,
        ..Serve::approved()
    };
    let first = pass(&temp, &mut ledger, &deferred, None, Some(&green));
    assert!(!first.enqueued(), "{}", first.calls);
    assert!(
        first
            .mutation
            .as_deref()
            .is_some_and(|mutation| mutation.starts_with("generated_path_batch:#77")),
        "{:?}\n{}",
        first.mutation,
        first.calls
    );
    assert_eq!(
        ledger.flake_requeue.requeued[&key()].state,
        ReceiptState::Pending
    );
    // Within the tick: still pending.
    let second = pass(&temp, &mut ledger, &deferred, None, Some(&green));
    assert!(!second.enqueued(), "{}", second.calls);
    assert_eq!(
        ledger
            .flake_requeue
            .pass
            .last()
            .map(|decision| decision.outcome),
        Some("requeue_pending")
    );
    // A tick later, the batch-mate gone: retried once and done.
    let receipt = ledger
        .flake_requeue
        .requeued
        .get_mut(&key())
        .expect("receipt");
    receipt.at = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
    let third = pass(&temp, &mut ledger, &Serve::approved(), None, Some(&green));
    assert!(third.enqueued(), "{:?}\n{}", third.mutation, third.calls);
    let receipt = &ledger.flake_requeue.requeued[&key()];
    assert_eq!((receipt.state, receipt.attempts), (ReceiptState::Done, 2));
}

#[cfg(unix)]
#[test]
fn an_unreadable_job_log_never_requeues() {
    let temp = tempfile::tempdir().expect("temp");
    let green = green_base(&["macos"]);
    let mut ledger = seeded();
    let serve = Serve {
        log: false,
        ..Serve::approved()
    };
    let pass = pass(&temp, &mut ledger, &serve, None, Some(&green));
    assert!(!pass.enqueued(), "{}", pass.calls);
    assert_eq!(
        pass.mutation.as_deref(),
        Some(REFUSED),
        "{:?}\n{}",
        pass.error,
        pass.calls
    );
    assert!(ledger.flake_requeue.requeued.is_empty());
    assert_eq!(ledger.flake_requeue.sightings.len(), 1);
}

#[cfg(unix)]
#[test]
fn a_defect_fix_removed_flake_head_is_never_requeued() {
    let temp = tempfile::tempdir().expect("temp");
    let green = green_base(&["macos"]);
    let mut ledger = seeded();
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
    let pass = pass(
        &temp,
        &mut ledger,
        &Serve::approved(),
        Some(&reading),
        Some(&green),
    );
    assert!(!pass.enqueued(), "{}", pass.calls);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("defect_fix_removed:defect-fix")
    );
    assert!(ledger.flake_requeue.requeued.is_empty());
}

/// The base tip must show a completed `success`: an empty or a pending tip
/// is not green, through the real path.
#[cfg(unix)]
#[test]
fn an_empty_or_pending_base_tip_never_requeues() {
    let green = green_base(&["macos"]);
    for tip in [TIP_EMPTY, TIP_PENDING] {
        let temp = tempfile::tempdir().expect("temp");
        let mut ledger = seeded();
        let serve = Serve {
            tip: tip.to_owned(),
            ..Serve::approved()
        };
        let pass = pass(&temp, &mut ledger, &serve, None, Some(&green));
        assert!(!pass.enqueued(), "{tip}: {}", pass.calls);
        assert_eq!(pass.mutation.as_deref(), Some(REFUSED), "{:?}", pass.error);
        assert!(ledger.flake_requeue.requeued.is_empty());
        assert_eq!(
            ledger
                .flake_requeue
                .pass
                .last()
                .map(|decision| decision.outcome),
            Some("base_unknown"),
            "{tip}"
        );
    }
}

/// The breaker with required approval: a re-ejected head stops at the
/// approval gate's ejection cap, so admission never sees it. The sweep reads
/// the second ejection on its own, trips the signature, and a corroborated
/// head with no receipt of its own is then refused through the real path.
#[cfg(unix)]
#[test]
fn the_sweep_trips_a_signature_whose_requeued_head_was_ejected_again() {
    let temp = tempfile::tempdir().expect("temp");
    let green = green_base(&["macos"]);
    let mut ledger = seeded();
    // First ejection, flake re-enqueue.
    let first = pass(&temp, &mut ledger, &Serve::approved(), None, Some(&green));
    assert!(first.enqueued(), "{:?}\n{}", first.mutation, first.calls);
    // The re-enqueue happened between the two removals.
    ledger
        .flake_requeue
        .requeued
        .get_mut(&key())
        .expect("receipt")
        .at = "2026-10-31T21:20:00Z".to_owned();
    // Second ejection, same test, another merge group.
    let again = Serve {
        removals: vec![
            ("2026-10-31T21:10:00Z", GROUP),
            ("2026-10-31T21:40:00Z", GROUP_2),
        ],
        ..Serve::approved()
    };
    // No admission pass here: with required approval the gate stops a head
    // the queue removed twice at the ejection cap, before the flake class
    // runs, so only the sweep (which runs before admissions and reads no
    // approval) can see the second ejection.
    // The sweep, as the pass runs it before admissions.
    let actions = fake_gh(&temp, &script(&temp, &again));
    let observation = observation_for(ready_pr(), true);
    let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
    crate::app::merge_steward_cmd::flake_requeue::sweep(
        &FlakePolicy {
            mode: FlakeMode::Apply,
            window_hours: 24,
            max_requeues: 3,
        },
        &observation,
        &mut ledger,
        &crate::app::merge_steward_cmd::flake_requeue::SweepReads {
            queue: &|pr| {
                crate::app::merge_steward_cmd::native_arm::read_queue_state(
                    &actions,
                    "owner/repo",
                    pr,
                )
            },
            assess: &|report| {
                crate::environment_requeue::assess_with(&run_gh, "owner/repo", report, true, None)
            },
        },
    );
    let trip = ledger
        .flake_requeue
        .tripped
        .values()
        .next()
        .expect("the signature tripped");
    assert_eq!(
        (trip.pr, trip.at.as_str(), trip.unread),
        (42, "2026-10-31T21:40:00Z", false)
    );
    assert!(ledger.flake_requeue.requeued[&key()].swept);
    // Another corroborated head on the signature, with no receipt of its
    // own, is refused.
    ledger.flake_requeue.requeued.clear();
    let third = pass(&temp, &mut ledger, &Serve::approved(), None, Some(&green));
    assert!(!third.enqueued(), "{}", third.calls);
    assert_eq!(third.mutation.as_deref(), Some(REFUSED));
    assert_eq!(
        ledger
            .flake_requeue
            .pass
            .last()
            .map(|decision| decision.outcome),
        Some("signature_tripped")
    );
}

/// The sweep fails closed: a re-ejection whose signature cannot be read
/// trips the signature, and a timeline it cannot read holds the signature
/// for the pass (`breaker_unread`) without tripping it.
#[cfg(unix)]
#[test]
fn the_sweep_trips_on_an_unread_re_ejection_and_holds_on_an_unread_timeline() {
    let temp = tempfile::tempdir().expect("temp");
    let again = Serve {
        removals: vec![
            ("2026-10-31T21:10:00Z", GROUP),
            ("2026-10-31T21:40:00Z", GROUP_2),
        ],
        ..Serve::approved()
    };
    let actions = fake_gh(&temp, &script(&temp, &again));
    let observation = observation_for(ready_pr(), true);
    let policy = FlakePolicy {
        mode: FlakeMode::Apply,
        window_hours: 24,
        max_requeues: 3,
    };
    let receipted = || {
        let mut ledger = StewardLedger::default();
        ledger.flake_requeue.requeued.insert(
            key(),
            crate::app::merge_steward_cmd::flake_requeue::Receipt {
                repo: "owner/repo".to_owned(),
                signature: format!("macos: {MIDI}"),
                ejection_at: "2026-10-31T21:10:00Z".to_owned(),
                at: "2026-10-31T21:20:00Z".to_owned(),
                state: ReceiptState::Done,
                attempts: 1,
                swept: false,
            },
        );
        ledger
    };
    let queue = |pr| {
        crate::app::merge_steward_cmd::native_arm::read_queue_state(&actions, "owner/repo", pr)
    };
    // The classifier reads nothing: tripped, marked unread.
    let mut ledger = receipted();
    crate::app::merge_steward_cmd::flake_requeue::sweep(
        &policy,
        &observation,
        &mut ledger,
        &crate::app::merge_steward_cmd::flake_requeue::SweepReads {
            queue: &queue,
            assess: &|_| None,
        },
    );
    let trip = ledger
        .flake_requeue
        .tripped
        .values()
        .next()
        .expect("tripped closed");
    assert!(trip.unread);
    // The timeline cannot be read: not tripped, held for the pass.
    let mut ledger = receipted();
    crate::app::merge_steward_cmd::flake_requeue::sweep(
        &policy,
        &observation,
        &mut ledger,
        &crate::app::merge_steward_cmd::flake_requeue::SweepReads {
            queue: &|_| Err("HTTP 502".to_owned()),
            assess: &|_| None,
        },
    );
    assert!(ledger.flake_requeue.tripped.is_empty());
    assert_eq!(ledger.flake_requeue.unswept.len(), 1);
    assert!(!ledger.flake_requeue.requeued[&key()].swept);
}

/// A readable queue response that lacks the removal, head, or timestamp
/// evidence needed to prove a second ejection keeps the receipt unswept.
#[cfg(unix)]
#[test]
fn the_sweep_holds_when_the_queue_report_has_no_removal() {
    let temp = tempfile::tempdir().expect("temp");
    let serve = Serve {
        removals: Vec::new(),
        ..Serve::approved()
    };
    let actions = fake_gh(&temp, &script(&temp, &serve));
    let observation = observation_for(ready_pr(), true);
    let mut ledger = StewardLedger::default();
    ledger.flake_requeue.requeued.insert(
        key(),
        Receipt {
            repo: "owner/repo".to_owned(),
            signature: format!("macos: {MIDI}"),
            ejection_at: "2026-10-31T21:10:00Z".to_owned(),
            at: "2026-10-31T21:20:00Z".to_owned(),
            state: ReceiptState::Done,
            attempts: 1,
            swept: false,
        },
    );
    let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
    crate::app::merge_steward_cmd::flake_requeue::sweep(
        &FlakePolicy {
            mode: FlakeMode::Apply,
            window_hours: 24,
            max_requeues: 3,
        },
        &observation,
        &mut ledger,
        &crate::app::merge_steward_cmd::flake_requeue::SweepReads {
            queue: &|pr| {
                crate::app::merge_steward_cmd::native_arm::read_queue_state(
                    &actions,
                    "owner/repo",
                    pr,
                )
            },
            assess: &|report| {
                crate::environment_requeue::assess_with(&run_gh, "owner/repo", report, true, None)
            },
        },
    );
    assert!(ledger.flake_requeue.tripped.is_empty());
    assert_eq!(ledger.flake_requeue.unswept.len(), 1);
    assert!(!ledger.flake_requeue.requeued[&key()].swept);
}
