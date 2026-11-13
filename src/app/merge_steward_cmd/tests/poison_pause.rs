//! The base-poison pause, the measured fix, and the in-order restore.
//!
//! The replay drives the steward's own decision functions through the
//! 2026-11-11 Pulp incident, in which `Enforce version & skill sync` began
//! failing on a date (a source-contract recheck fell due on 2026-11-10):
//!
//! * 23:40Z `#11999` fails `macos` on its own (`WakeSignal` race), and
//!   `#12012` stacked behind it fails too. One pull request's failure.
//! * 00:04Z, 00:18Z, 00:46Z: groups for `#12012`, `#11890` and `#12008`, each
//!   on `main`, each fail `Enforce version & skill sync`.
//! * 01:33Z `#12025` (the recheck) opens; its head passes the context while
//!   the other open heads fail it.
//! * The fix's group passes; the paused entries go back in their order.
//!
//! The same timeline is replayed with the detector's guard removed to show
//! the replay would notice: a single pull request's failure must not pause.
use std::collections::{BTreeMap, BTreeSet};

use chrono::TimeZone as _;

use super::*;
use crate::app::merge_steward_cmd::base_poison::{self, RepoPoison};
use crate::app::merge_steward_cmd::poison_pause::{self, Verdict};
use crate::base_health::merge_group::{GroupCheck, MergeGroup, MergeGroupHealth, detect};
use crate::base_health::{AutoJump, BasePoisonPolicy, PoisonHold};

const CONTEXT: &str = "Enforce version & skill sync";
const REPO: &str = "Generous-Corp/pulp";

/// Hours and minutes after 2026-11-10T00:00Z (so `at(24, 4)` is 00:04Z on
/// the 11th).
fn at(hour: i64, minute: i64) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 10, 0, 0, 0)
        .single()
        .expect("time")
        + chrono::Duration::hours(hour)
        + chrono::Duration::minutes(minute)
}

fn group(
    sha: char,
    prs: &[u64],
    checks: &[(&str, &str)],
    done: chrono::DateTime<Utc>,
) -> MergeGroup {
    MergeGroup {
        head_sha: sha.to_string().repeat(40),
        prs: prs.iter().copied().collect::<BTreeSet<_>>(),
        checks: checks
            .iter()
            .enumerate()
            .map(|(index, (context, conclusion))| GroupCheck {
                context: (*context).to_owned(),
                conclusion: (*conclusion).to_owned(),
                check_id: Some(1000 + index as u64),
                completed_at: Some(done),
                title: None,
            })
            .collect(),
    }
}

fn required() -> Vec<String> {
    vec![CONTEXT.to_owned(), "macos".to_owned()]
}

/// An open pull request as the census reads it.
fn open_pr(
    number: u64,
    head: char,
    position: Option<u64>,
    context: Option<(&str, &str)>,
) -> ObservedPr {
    let mut rollup = vec![serde_json::json!({
        "__typename": "CheckRun", "name": "macos", "status": "COMPLETED",
        "conclusion": "SUCCESS", "completedAt": "2026-11-11T00:30:00Z"
    })];
    if let Some((conclusion, completed)) = context {
        rollup.push(serde_json::json!({
            "__typename": "CheckRun", "name": CONTEXT, "status": "COMPLETED",
            "conclusion": conclusion, "completedAt": completed
        }));
    }
    let positions = position.map_or_else(BTreeMap::new, |position| {
        BTreeMap::from([(number, position)])
    });
    parse_pr(
        &serde_json::json!({
            "id": format!("PR_{number}"), "number": number, "state": "OPEN", "isDraft": false,
            "baseRefName": "main", "headRefOid": head.to_string().repeat(40),
            "headRefName": format!("branch-{number}"), "mergeStateStatus": "CLEAN",
            "autoMergeRequest": null, "labels": [], "statusCheckRollup": rollup,
        }),
        &positions,
    )
    .expect("PR")
}

fn observation(prs: Vec<ObservedPr>) -> RepoObservation {
    let mut observation = observation_for(prs[0].clone(), true);
    observation.repo = REPO.to_owned();
    observation.prs = prs;
    observation
}

fn poison(health: MergeGroupHealth, hold: PoisonHold, prs: &[ObservedPr]) -> RepoPoison {
    let (fixes, candidates) = base_poison::assemble_fixes(BTreeMap::new(), prs, &health);
    RepoPoison {
        policy: BasePoisonPolicy {
            hold,
            control_workflows: std::collections::BTreeMap::new(),
            pause: hold,
            auto_jump: AutoJump::On,
            read_error: None,
        },
        fixes,
        tip: base_poison::red_tip(&health),
        health,
        fix_read_error: None,
        fix_ambiguity: None,
        candidates,
    }
}

fn queued_decision(pr: &ObservedPr) -> StewardDecision {
    match pr.fact.queue_position {
        Some(position) => StewardDecision::Queued { position },
        None => StewardDecision::ArmMergeQueue,
    }
}

/// Merge groups newest first, as the detector reads them, at each moment.
fn groups_at_0015() -> Vec<MergeGroup> {
    vec![
        group(
            'e',
            &[12012],
            &[(CONTEXT, "failure"), ("macos", "cancelled")],
            at(24, 14),
        ),
        group(
            'd',
            &[11999, 12012],
            &[(CONTEXT, "success"), ("macos", "failure")],
            at(24, 5),
        ),
        group(
            'c',
            &[11999],
            &[(CONTEXT, "success"), ("macos", "failure")],
            at(24, 3),
        ),
    ]
}

fn groups_at_0030() -> Vec<MergeGroup> {
    let mut groups = groups_at_0015();
    groups.insert(
        0,
        group(
            'f',
            &[11890],
            &[(CONTEXT, "failure"), ("macos", "cancelled")],
            at(24, 28),
        ),
    );
    groups
}

#[test]
fn one_pull_requests_own_failure_never_pauses_the_queue() {
    // 00:15Z: `macos` failed twice, but both groups carried #11999, and the
    // version check failed once. Nothing is poisoned.
    let health = detect(&groups_at_0015(), &required(), at(24, 15));
    assert!(
        matches!(health, MergeGroupHealth::Clear { .. }),
        "{health:?}"
    );
    let queued = open_pr(
        12020,
        'a',
        Some(1),
        Some(("SUCCESS", "2026-11-11T00:10:00Z")),
    );
    let poison = poison(health, PoisonHold::On, std::slice::from_ref(&queued));
    assert_eq!(
        poison_pause::verdict(Some(&poison), &queued.fact, &queued_decision(&queued)),
        Verdict::Keep
    );
    assert_eq!(
        base_poison::admission(Some(&poison), 12020),
        base_poison::Admission::Proceed
    );
}

#[test]
fn the_guard_against_a_shared_pull_request_is_what_keeps_the_queue_running() {
    // Negative control: drop the shared #11999 from one `macos` group and the
    // same two failures read as poison. The test above passes only because
    // the detector requires groups with no pull request common to all.
    let mut groups = groups_at_0015();
    groups[1].prs = BTreeSet::from([12012]);
    let health = detect(&groups, &required(), at(24, 15));
    assert_eq!(health.poisoned_contexts(), vec!["macos".to_owned()]);
}

#[test]
#[allow(clippy::too_many_lines)] // One incident, start to finish, reads best in one place.
fn replay_2026_11_11_pause_fix_first_and_restore_in_order() {
    // 00:30Z: the version check failed in two consecutive groups carrying
    // different pull requests. Two entries sit in the queue.
    let health = detect(&groups_at_0030(), &required(), at(24, 30));
    assert_eq!(health.poisoned_contexts(), vec![CONTEXT.to_owned()]);
    let first = open_pr(
        12028,
        'a',
        Some(1),
        Some(("SUCCESS", "2026-11-10T22:00:00Z")),
    );
    let second = open_pr(
        12030,
        'b',
        Some(2),
        Some(("SUCCESS", "2026-11-10T22:10:00Z")),
    );
    let red_a = open_pr(12013, 'c', None, Some(("FAILURE", "2026-11-11T00:40:00Z")));
    let red_b = open_pr(12014, 'd', None, Some(("FAILURE", "2026-11-11T00:50:00Z")));
    // No fix is open yet: nothing is fast-tracked, so the root cause is
    // routed to a tracking issue (see the next test), and the queue pauses.
    let prs = vec![first.clone(), second.clone(), red_a.clone(), red_b.clone()];
    let early = poison(health.clone(), PoisonHold::On, &prs);
    assert!(early.fixes.is_empty(), "{:?}", early.fixes);
    for (pr, position) in [(&first, 1), (&second, 2)] {
        assert_eq!(
            poison_pause::verdict(Some(&early), &pr.fact, &queued_decision(pr)),
            Verdict::Pause {
                contexts: CONTEXT.to_owned(),
                position,
            }
        );
    }
    // Dry-run pauses nothing; it only reports.
    let dry = poison(health.clone(), PoisonHold::DryRun, &prs);
    assert!(matches!(
        poison_pause::verdict(Some(&dry), &first.fact, &queued_decision(&first)),
        Verdict::WouldPause { .. }
    ));

    // The pause is recorded in queue order (recorded out of order on purpose).
    let mut ledger = StewardLedger::default();
    let observed = observation(prs.clone());
    poison_pause::record(&mut ledger, &observed, &second, 2, CONTEXT, at(24, 31));
    poison_pause::record(&mut ledger, &observed, &first, 1, CONTEXT, at(24, 31));
    // Nothing is restored while the base is still poisoned.
    assert_eq!(
        poison_pause::restore_rank(&ledger, REPO, "main", 12028),
        None
    );
    let entries = &ledger.poison_paused[&poison_pause::ledger_key(REPO, "main")];
    assert_eq!(
        entries.iter().map(|entry| entry.pr).collect::<Vec<_>>(),
        vec![12028, 12030]
    );

    // 01:40Z: #12025 opens and its head passes the context after the
    // newest failing group, while #12013 and #12014 still fail it.
    let mut health = health;
    if let MergeGroupHealth::Poisoned { contexts } = &mut health {
        contexts[0].latest_failure_at = Some(at(24, 28));
    }
    let fix = open_pr(12025, 'f', None, Some(("SUCCESS", "2026-11-11T01:40:00Z")));
    let paused = |prs: &[&ObservedPr]| {
        prs.iter()
            .map(|pr| {
                let mut pr = (*pr).clone();
                pr.fact.queue_position = None;
                pr
            })
            .collect::<Vec<_>>()
    };
    // Census order deliberately differs from queue order.
    let mut prs = vec![red_a.clone(), red_b.clone(), fix.clone()];
    prs.extend(paused(&[&second, &first]));
    let with_fix = poison(health.clone(), PoisonHold::On, &prs);
    // Measured evidence exempts and admits the fix; it never fast-tracks or
    // jumps (a path-filtered check can pass on an unrelated head).
    assert!(with_fix.fixes.is_empty(), "{:?}", with_fix.fixes);
    assert_eq!(
        with_fix.candidates.iter().copied().collect::<Vec<_>>(),
        vec![12025]
    );
    assert_eq!(
        base_poison::admission(Some(&with_fix), 12025),
        base_poison::Admission::Proceed
    );
    // The fix itself is never paused, even when it sits in the queue.
    let mut queued_fix = fix.clone();
    queued_fix.fact.queue_position = Some(1);
    assert_eq!(
        poison_pause::verdict(
            Some(&with_fix),
            &queued_fix.fact,
            &queued_decision(&queued_fix)
        ),
        Verdict::Keep
    );
    for held in [12028, 12030, 12013] {
        assert!(matches!(
            base_poison::admission(Some(&with_fix), held),
            base_poison::Admission::Hold { .. }
        ));
    }
    // While poisoned the paused entries keep waiting, even one somebody put
    // back in the queue by hand (the next pass pauses it again).
    let mut by_hand = prs.clone();
    by_hand[4].fact.queue_position = Some(1);
    let lines = poison_pause::reconcile(
        &mut ledger,
        &observation(by_hand),
        Some(&with_fix),
        at(25, 45),
    );
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines
            .iter()
            .all(|line| line.contains("restores when a merge group passes")),
        "{lines:?}"
    );

    // 05:37Z: the fix's group passes the context. The poison clears.
    let mut groups = groups_at_0030();
    groups.insert(
        0,
        group(
            '9',
            &[12025],
            &[(CONTEXT, "success"), ("macos", "success")],
            at(29, 37),
        ),
    );
    let clear = detect(&groups, &required(), at(29, 38));
    assert!(matches!(clear, MergeGroupHealth::Clear { .. }), "{clear:?}");
    let cleared = poison(clear, PoisonHold::On, &prs);
    for pr in &prs {
        assert_eq!(
            poison_pause::verdict(Some(&cleared), &pr.fact, &queued_decision(pr)),
            Verdict::Keep
        );
        assert_eq!(
            base_poison::admission(Some(&cleared), pr.fact.number),
            base_poison::Admission::Proceed
        );
    }
    // The pass that reads the base clear marks them for restore; they are
    // then admitted first, in their queue order, ahead of every other
    // approved head.
    assert_eq!(
        poison_pause::restore_rank(&ledger, REPO, "main", 12028),
        None
    );
    let lines = poison_pause::reconcile(
        &mut ledger,
        &observation(prs.clone()),
        Some(&cleared),
        at(29, 38),
    );
    assert!(
        lines.iter().all(|line| line.contains("restoring first")),
        "{lines:?}"
    );
    let decisions = prs
        .iter()
        .map(|_| StewardDecision::ArmMergeQueue)
        .collect::<Vec<_>>();
    let order = crate::app::merge_steward_cmd::pass_budget::processing_order(
        REPO, "main", &prs, &decisions, &ledger,
    );
    let numbers = order
        .iter()
        .map(|index| prs[*index].fact.number)
        .collect::<Vec<_>>();
    assert_eq!(&numbers[..2], &[12028, 12030], "{numbers:?}");

    // Once both are back in the queue the ledger forgets them.
    let mut requeued = prs.clone();
    requeued[4].fact.queue_position = Some(1);
    requeued[3].fact.queue_position = Some(2);
    let lines = poison_pause::reconcile(
        &mut ledger,
        &observation(requeued),
        Some(&cleared),
        at(29, 50),
    );
    assert_eq!(
        lines,
        vec!["#12028 restored".to_owned(), "#12030 restored".to_owned()]
    );
    assert!(ledger.poison_paused.is_empty());
}

#[test]
fn a_paused_entry_whose_head_moved_or_closed_is_not_restored() {
    let first = open_pr(12028, 'a', Some(1), None);
    let second = open_pr(12030, 'b', Some(2), None);
    let mut ledger = StewardLedger::default();
    let observed = observation(vec![first.clone(), second.clone()]);
    poison_pause::record(&mut ledger, &observed, &first, 1, CONTEXT, at(24, 31));
    poison_pause::record(&mut ledger, &observed, &second, 2, CONTEXT, at(24, 31));
    let mut moved = first.clone();
    moved.fact.head_sha = "e".repeat(40);
    moved.fact.queue_position = None;
    let lines = poison_pause::reconcile(&mut ledger, &observation(vec![moved]), None, at(25, 0));
    assert_eq!(
        lines,
        vec!["#12028 head_moved".to_owned(), "#12030 closed".to_owned()]
    );
    assert!(ledger.poison_paused.is_empty());
}

#[test]
fn a_paused_entry_is_forgotten_after_its_restore_window() {
    let first = open_pr(12028, 'a', Some(1), None);
    let mut ledger = StewardLedger::default();
    poison_pause::record(
        &mut ledger,
        &observation(vec![first.clone()]),
        &first,
        1,
        CONTEXT,
        at(0, 0),
    );
    let mut waiting = first;
    waiting.fact.queue_position = None;
    let kept = poison_pause::reconcile(
        &mut ledger,
        &observation(vec![waiting.clone()]),
        None,
        at(47, 0),
    );
    assert_eq!(kept.len(), 1);
    assert!(kept[0].contains("restoring first"), "{kept:?}");
    let gone = poison_pause::reconcile(&mut ledger, &observation(vec![waiting]), None, at(49, 0));
    assert_eq!(gone, vec!["#12028 expired".to_owned()]);
}

#[test]
fn a_passing_head_is_only_a_fix_when_other_heads_fail_after_the_poison() {
    let mut health = detect(&groups_at_0030(), &required(), at(24, 30));
    if let MergeGroupHealth::Poisoned { contexts } = &mut health {
        contexts[0].latest_failure_at = Some(at(24, 28));
    }
    let fix = open_pr(12025, 'f', None, Some(("SUCCESS", "2026-11-11T01:40:00Z")));
    let stale = open_pr(12001, 'a', None, Some(("SUCCESS", "2026-11-10T20:00:00Z")));
    let red = open_pr(12013, 'c', None, Some(("FAILURE", "2026-11-11T00:40:00Z")));
    let red_b = open_pr(12014, 'd', None, Some(("FAILURE", "2026-11-11T00:50:00Z")));
    // One failing head is not enough to say the check is sensitive at heads
    // (a `macos`-style context passes its fast tier at every head).
    let lone = poison_pause::passing_head_fixes(&[fix.clone(), red.clone()], &health);
    assert!(lone.is_empty(), "{lone:?}");
    let named = poison_pause::passing_head_fixes(&[fix, stale, red, red_b], &health);
    // A head that last passed before the base broke proves nothing.
    assert_eq!(named.keys().copied().collect::<Vec<_>>(), vec![12025]);
    // A measured candidate is never the fast-tracked fix: only named ones are.
    let named_only = BTreeMap::from([(
        12999,
        vec![format!("mentions red required check `{CONTEXT}`")],
    )]);
    let prs = [
        open_pr(12025, 'f', None, Some(("SUCCESS", "2026-11-11T01:40:00Z"))),
        open_pr(12013, 'c', None, Some(("FAILURE", "2026-11-11T00:40:00Z"))),
        open_pr(12014, 'd', None, Some(("FAILURE", "2026-11-11T00:50:00Z"))),
    ];
    let (chosen, candidates) = base_poison::assemble_fixes(named_only, &prs, &health);
    assert_eq!(chosen.keys().copied().collect::<Vec<_>>(), vec![12999]);
    assert_eq!(
        candidates.into_iter().collect::<Vec<_>>(),
        vec![12025, 12999]
    );
    let _ = named;
}

#[test]
fn with_no_fix_named_the_root_cause_gets_one_tracking_issue_first_in_priority() {
    let temp = tempfile::tempdir().expect("temp");
    let health = detect(&groups_at_0030(), &required(), at(24, 30));
    let poisoned = health.poisoned()[0].clone();
    let calls = std::cell::RefCell::new(Vec::<String>::new());
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let line = argv.join(" ");
        calls.borrow_mut().push(line.clone());
        if line.contains("--method POST") {
            Ok(r#"{"number":777}"#.to_owned())
        } else {
            Ok(String::new())
        }
    };
    let priority = temp.path().join("priority.txt");
    fs::write(&priority, "Generous-Corp/pulp#12044\n").expect("seed");
    let hooks = base_poison::ReviewHooks {
        kick: Vec::new(),
        priority_file: Some(priority.clone()),
    };
    let state = temp.path().join("state");
    let lines =
        base_poison::route_root_cause(&run_gh, &hooks, &state, REPO, "main", &poisoned, at(24, 31));
    assert!(lines[0].contains("#777: opened"), "{lines:?}");
    assert_eq!(
        fs::read_to_string(&priority).expect("read"),
        "Generous-Corp/pulp#777\nGenerous-Corp/pulp#12044\n"
    );
    let posted = calls
        .borrow()
        .iter()
        .filter(|line| line.contains("--method POST"))
        .count();
    assert_eq!(posted, 1);
    let body = calls
        .borrow()
        .iter()
        .find(|line| line.contains("--method POST"))
        .cloned()
        .unwrap_or_default();
    assert!(
        body.contains(&base_poison::issue_key(REPO, "main", CONTEXT)),
        "{body}"
    );
    // The next pass reuses the receipt: no second issue, no second listing.
    let before = calls.borrow().len();
    let again =
        base_poison::route_root_cause(&run_gh, &hooks, &state, REPO, "main", &poisoned, at(24, 40));
    assert!(again[0].contains("#777: recorded"), "{again:?}");
    assert_eq!(calls.borrow().len(), before);
}

#[test]
fn an_open_tracking_issue_is_found_by_its_marker_not_duplicated() {
    let temp = tempfile::tempdir().expect("temp");
    let health = detect(&groups_at_0030(), &required(), at(24, 30));
    let poisoned = health.poisoned()[0].clone();
    let key = base_poison::issue_key(REPO, "main", CONTEXT);
    let listed = serde_json::json!({
        "number": 640, "body": format!("x\n{}{key} -->", base_poison::ISSUE_MARKER)
    })
    .to_string();
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let line = argv.join(" ");
        assert!(!line.contains("--method POST"), "must not open a duplicate");
        Ok(if line.contains("issues?state=open") {
            listed.clone()
        } else {
            String::new()
        })
    };
    let found = base_poison::ensure_tracking_issue(
        &run_gh,
        temp.path(),
        REPO,
        "main",
        &poisoned,
        at(24, 31),
    );
    assert_eq!(found, Ok((640, "already open")));
}

#[test]
fn a_poisoned_pass_dequeues_through_the_audited_path_and_records_the_place() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let guard = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/ghapp_queue_removal_guard.py");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"dequeuePullRequest"*)
    printf 'marker=%s\n' "${{SHIPYARD_INTERNAL_QUEUE_MUTATION:-}}" >> '{log}'
    python3 -I '{guard}' "$@" 2>> '{log}' || exit 97
    printf '%s' '{{"data":{{"dequeuePullRequest":{{"mergeQueueEntry":{{"id":"MQE_1"}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
            guard = guard.display(),
        ),
    );
    let queued = open_pr(12028, 'a', Some(1), None);
    let health = detect(&groups_at_0030(), &required(), at(24, 30));
    let observed = observation(vec![queued.clone()]);
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observed, &ledger_path, &control);
    let mut ledger = StewardLedger::default();
    let run = |hold, apply, ledger: &mut StewardLedger| {
        let poison = poison(health.clone(), hold, std::slice::from_ref(&queued));
        poison_pause::pause_pr(
            Some(&context),
            apply,
            &observed,
            &queued,
            &StewardDecision::Queued { position: 1 },
            Some(&poison),
            ledger,
        )
    };
    // Dry-run and a non-applying pass never touch the queue.
    assert!(run(PoisonHold::DryRun, true, &mut ledger).is_none());
    let report = run(PoisonHold::On, false, &mut ledger).expect("report");
    assert!(
        report
            .mutation
            .as_deref()
            .is_some_and(|m| m.starts_with("would_pause_base_poisoned:"))
    );
    assert!(fs::read_to_string(&log).unwrap_or_default().is_empty());
    // On: dequeued on Shipyard's internal path, accepted by the real guard.
    let report = run(PoisonHold::On, true, &mut ledger).expect("report");
    assert_eq!(report.error, None);
    assert_eq!(
        report.mutation.as_deref(),
        Some(format!("paused_base_poisoned:{}", "a".repeat(40)).as_str())
    );
    let calls = fs::read_to_string(&log).expect("calls");
    assert!(calls.contains("marker=1"), "{calls}");
    assert!(!calls.contains("refusing"), "{calls}");
    assert_eq!(
        ledger.poison_paused[&poison_pause::ledger_key(REPO, "main")][0].pr,
        12028
    );
    // A steward removal never counts toward the head's ejection cap.
    assert_eq!(ledger.steward_removals.len(), 1);
    assert!(
        ledger
            .audit
            .iter()
            .any(|entry| entry.action == poison_pause::PAUSE_AUDIT)
    );
}

#[test]
fn a_stale_or_unread_poison_restores_nothing_but_the_hold_off_does() {
    let first = open_pr(12028, 'a', Some(1), None);
    let mut ledger = StewardLedger::default();
    poison_pause::record(
        &mut ledger,
        &observation(vec![first.clone()]),
        &first,
        1,
        CONTEXT,
        at(24, 31),
    );
    let mut waiting = first;
    waiting.fact.queue_position = None;
    let prs = vec![waiting];
    let reading = |health, hold| poison(health, hold, &prs);
    for health in [
        MergeGroupHealth::Clear {
            detail: "`x` streak is stale (newest failure older than 2 h)".to_owned(),
            passed: Vec::new(),
            passed_by: std::collections::BTreeMap::new(),
        },
        MergeGroupHealth::Unknown {
            detail: "HTTP 502".to_owned(),
        },
    ] {
        poison_pause::reconcile(
            &mut ledger,
            &observation(prs.clone()),
            Some(&reading(health, PoisonHold::On)),
            at(27, 0),
        );
        assert_eq!(
            poison_pause::restore_rank(&ledger, REPO, "main", 12028),
            None
        );
    }
    let off = reading(
        MergeGroupHealth::Unknown {
            detail: "x".to_owned(),
        },
        PoisonHold::Off,
    );
    poison_pause::reconcile(
        &mut ledger,
        &observation(prs.clone()),
        Some(&off),
        at(27, 5),
    );
    assert_eq!(
        poison_pause::restore_rank(&ledger, REPO, "main", 12028),
        Some(0)
    );
}

#[test]
fn the_pause_defaults_to_shadow_and_skips_entries_the_steward_did_not_admit() {
    let health = detect(&groups_at_0030(), &required(), at(24, 30));
    let queued = open_pr(12028, 'a', Some(1), None);
    let mut reading = poison(health, PoisonHold::On, std::slice::from_ref(&queued));
    // `merge_group_pause` unset: hold on alone only reports.
    reading.policy.pause = crate::base_health::BasePoisonPolicy::for_base(true).pause;
    assert!(matches!(
        poison_pause::verdict(Some(&reading), &queued.fact, &queued_decision(&queued)),
        Verdict::WouldPause { .. }
    ));
    reading.policy.pause = PoisonHold::On;
    // An opted-out entry is not the steward's to remove or restore.
    let opted = StewardDecision::OptedOut;
    assert_eq!(
        poison_pause::verdict(Some(&reading), &queued.fact, &opted),
        Verdict::Keep
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Dispatch, read, settle and gate in one place.
fn the_shadow_ledger_labels_episodes_by_a_pr_free_control_run() {
    use crate::app::merge_steward_cmd::shadow_ledger::{self, Episode};
    let key = "generous-corp/pulp@main";
    let firing = |strict: bool| {
        let mut map = BTreeMap::new();
        map.insert((CONTEXT.to_owned(), "streak".to_owned()), 2);
        if strict {
            map.insert((CONTEXT.to_owned(), "strict".to_owned()), 2);
        }
        map
    };
    let controls = BTreeMap::from([(CONTEXT.to_owned(), "version-skill-check.yml".to_owned())]);
    let held = BTreeSet::from([12028, 12030]);
    let none = BTreeSet::new();
    let calls = std::cell::RefCell::new(Vec::<String>::new());
    let verdict = std::cell::RefCell::new("");
    let tip = "1".repeat(40);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let line = argv.join(" ");
        calls.borrow_mut().push(line.clone());
        if line.contains("/dispatches") {
            assert!(line.contains("ref=main"), "{line}");
            return Ok(r#"{"workflow_run_id":42}"#.to_owned());
        }
        if line.ends_with("--jq .sha") {
            return Ok(tip.clone());
        }
        if line.contains("/check-runs?check_name=Enforce%20version%20%26%20skill%20sync") {
            let conclusion = *verdict.borrow();
            if conclusion.is_empty() {
                return Ok(r#"{"check_runs":[]}"#.to_owned());
            }
            return Ok(format!(
                r#"{{"check_runs":[{{"status":"completed","conclusion":"{conclusion}","started_at":"2026-11-12T23:00:00Z","check_suite":{{"workflow_run":{{"id":42}}}}}}]}}"#
            ));
        }
        Err(format!("unexpected: {line}"))
    };
    // 00:30Z the streak and strict rules fire together: one control is
    // dispatched for the context, shared by both.
    let mut open: Vec<Episode> = Vec::new();
    shadow_ledger::advance(&mut open, key, &firing(true), &held, &none, at(24, 30));
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(24, 30));
    let dispatches = calls
        .borrow()
        .iter()
        .filter(|line| line.contains("/dispatches"))
        .count();
    assert_eq!(dispatches, 1);
    assert!(
        open.iter()
            .all(|episode| episode.control.as_ref().is_some_and(|c| c.base_sha == tip))
    );
    // The groups recover before the control reports: the episodes end but
    // wait for their label; nothing settles unlabelled.
    shadow_ledger::advance(&mut open, key, &BTreeMap::new(), &held, &none, at(24, 50));
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(24, 50));
    assert!(shadow_ledger::settle(&mut open, key).is_empty());
    // The control fails on the base without any pull request: a true fire.
    *verdict.borrow_mut() = "failure";
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(25, 0));
    let real = shadow_ledger::settle(&mut open, key);
    assert_eq!(real.len(), 2);
    assert!(
        real.iter()
            .all(|episode| episode.corroborated == Some(true)),
        "{real:?}"
    );
    // A later flake: the control passes on the base, so it is a false fire,
    // however long the streak looked.
    *verdict.borrow_mut() = "success";
    shadow_ledger::advance(&mut open, key, &firing(false), &held, &none, at(30, 0));
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(30, 0));
    shadow_ledger::advance(&mut open, key, &BTreeMap::new(), &held, &none, at(30, 30));
    let flake = shadow_ledger::settle(&mut open, key);
    assert_eq!(flake.len(), 1);
    assert_eq!(flake[0].corroborated, Some(false));
    // A context with no control workflow is unlabelled and counts toward
    // no gate.
    let mut other: Vec<Episode> = Vec::new();
    shadow_ledger::advance(&mut other, key, &firing(false), &held, &none, at(31, 0));
    shadow_ledger::label(
        &mut other,
        key,
        REPO,
        "main",
        &BTreeMap::new(),
        &run_gh,
        at(31, 0),
    );
    shadow_ledger::advance(&mut other, key, &BTreeMap::new(), &held, &none, at(31, 30));
    let unlabelled = shadow_ledger::settle(&mut other, key);
    assert_eq!(unlabelled[0].corroborated, None);

    let mut history: Vec<Episode> = real.into_iter().chain(flake).chain(unlabelled).collect();
    let gate = shadow_ledger::gate("streak", &history, at(32, 0));
    assert_eq!(gate["fires"], 2, "{gate}");
    assert_eq!(gate["unlabelled"], 1, "{gate}");
    assert_eq!(gate["precision"], 0.5);
    assert_eq!(gate["eligible_to_enforce"], false);
    // Ten control-confirmed strict fires across two weeks pass the gate...
    let strict = history
        .iter()
        .find(|episode| episode.rule == "strict")
        .cloned()
        .expect("strict");
    for day in 0..10 {
        let mut copy = strict.clone();
        copy.started_at =
            (at(0, 0) - chrono::Duration::days(15) + chrono::Duration::days(day)).to_rfc3339();
        copy.last_seen_at.clone_from(&copy.started_at);
        copy.ended_at = Some(copy.started_at.clone());
        history.push(copy);
    }
    assert_eq!(
        shadow_ledger::gate("strict", &history, at(32, 0))["eligible_to_enforce"],
        true
    );
    // ...and the same record below precision 0.9 does not.
    for _ in 0..3 {
        let mut miss = strict.clone();
        miss.corroborated = Some(false);
        history.push(miss);
    }
    assert_eq!(
        shadow_ledger::gate("strict", &history, at(32, 0))["eligible_to_enforce"],
        false
    );
}

#[test]
fn a_control_that_never_reports_leaves_the_episode_unlabelled() {
    use crate::app::merge_steward_cmd::shadow_ledger::{self, Episode};
    let key = "generous-corp/pulp@main";
    let controls = BTreeMap::from([(CONTEXT.to_owned(), "version-skill-check.yml".to_owned())]);
    let firing = BTreeMap::from([((CONTEXT.to_owned(), "streak".to_owned()), 2)]);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let line = argv.join(" ");
        Ok(if line.ends_with("--jq .sha") {
            "2".repeat(40)
        } else if line.contains("/dispatches") {
            r#"{"workflow_run_id":42}"#.to_owned()
        } else {
            r#"{"check_runs":[]}"#.to_owned()
        })
    };
    let mut open: Vec<Episode> = Vec::new();
    let none = BTreeSet::new();
    // A run of the context that started just before the dispatch is not the
    // control and must not label anything.
    let stale = |argv: &[String]| -> Result<String, String> {
        let line = argv.join(" ");
        Ok(if line.ends_with("--jq .sha") {
            "2".repeat(40)
        } else if line.contains("/dispatches") {
            r#"{"workflow_run_id":42}"#.to_owned()
        } else {
            r#"{"check_runs":[{"status":"completed","conclusion":"failure","started_at":"2026-11-10T23:59:30Z"}]}"#.to_owned()
        })
    };
    let mut old: Vec<Episode> = Vec::new();
    shadow_ledger::advance(&mut old, key, &firing, &none, &none, at(24, 0));
    shadow_ledger::label(&mut old, key, REPO, "main", &controls, &stale, at(24, 0));
    assert_eq!(old[0].corroborated, None);
    shadow_ledger::advance(&mut open, key, &firing, &none, &none, at(24, 0));
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(24, 0));
    shadow_ledger::advance(&mut open, key, &BTreeMap::new(), &none, &none, at(24, 10));
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(29, 0));
    assert!(shadow_ledger::settle(&mut open, key).is_empty());
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(31, 0));
    let done = shadow_ledger::settle(&mut open, key);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].corroborated, None);
}

#[test]
fn a_timed_out_control_leaves_the_episode_unlabelled() {
    use crate::app::merge_steward_cmd::shadow_ledger::{self, Episode};
    let key = "generous-corp/pulp@main";
    let controls = BTreeMap::from([(CONTEXT.to_owned(), "version-skill-check.yml".to_owned())]);
    let firing = BTreeMap::from([((CONTEXT.to_owned(), "streak".to_owned()), 2)]);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let line = argv.join(" ");
        Ok(if line.ends_with("--jq .sha") {
            "3".repeat(40)
        } else if line.contains("/dispatches") {
            r#"{"workflow_run_id":42}"#.to_owned()
        } else if line.contains("/check-runs?") {
            r#"{"check_runs":[{"status":"completed","conclusion":"timed_out","started_at":"2026-11-12T23:00:00Z","check_suite":{"workflow_run":{"id":42}}}]}"#.to_owned()
        } else {
            String::new()
        })
    };
    let mut open: Vec<Episode> = Vec::new();
    let none = BTreeSet::new();
    shadow_ledger::advance(&mut open, key, &firing, &none, &none, at(24, 0));
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(24, 0));
    shadow_ledger::advance(&mut open, key, &BTreeMap::new(), &none, &none, at(24, 10));
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(24, 10));
    let done = shadow_ledger::settle(&mut open, key);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].corroborated, None);
}

#[test]
fn overlapping_episodes_dispatch_distinct_controls() {
    use crate::app::merge_steward_cmd::shadow_ledger::{self, Episode};
    let key = "generous-corp/pulp@main";
    let controls = BTreeMap::from([(CONTEXT.to_owned(), "version-skill-check.yml".to_owned())]);
    let firing = BTreeMap::from([((CONTEXT.to_owned(), "streak".to_owned()), 2)]);
    let first_sha = "1".repeat(40);
    let second_sha = "2".repeat(40);
    let tips = std::cell::RefCell::new(vec![first_sha.clone(), second_sha.clone()]);
    let first_poll = std::cell::Cell::new(true);
    let dispatches = std::cell::RefCell::new(0usize);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let line = argv.join(" ");
        if line.ends_with("--jq .sha") {
            return Ok(tips.borrow_mut().remove(0));
        }
        if line.contains("/dispatches") {
            *dispatches.borrow_mut() += 1;
            let id = 40 + *dispatches.borrow();
            return Ok(format!(r#"{{"workflow_run_id":{id}}}"#));
        }
        if line.contains("/check-runs?") {
            if line.contains(&format!("commits/{first_sha}/")) && first_poll.replace(false) {
                return Ok(r#"{"check_runs":[]}"#.to_owned());
            }
            let conclusion = if line.contains(&format!("commits/{first_sha}/")) {
                "failure"
            } else {
                "success"
            };
            let id = if line.contains(&format!("commits/{first_sha}/")) {
                41
            } else {
                42
            };
            return Ok(serde_json::json!({
                "check_runs": [{
                    "status": "completed",
                    "conclusion": conclusion,
                    "started_at": "2026-11-12T23:00:00Z",
                    "check_suite": {"workflow_run": {"id": id}},
                }]
            })
            .to_string());
        }
        Err(format!("unexpected: {line}"))
    };
    let mut open: Vec<Episode> = Vec::new();
    let none = BTreeSet::new();
    shadow_ledger::advance(&mut open, key, &firing, &none, &none, at(24, 0));
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(24, 0));
    shadow_ledger::advance(&mut open, key, &BTreeMap::new(), &none, &none, at(24, 5));
    shadow_ledger::advance(&mut open, key, &firing, &none, &none, at(24, 10));
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(24, 10));
    assert_eq!(*dispatches.borrow(), 2);
    assert_eq!(open[0].corroborated, Some(true));
    assert_eq!(open[1].corroborated, Some(false));
    let controls = open
        .iter()
        .filter_map(|episode| episode.control.as_ref())
        .map(|control| control.base_sha.clone())
        .collect::<Vec<_>>();
    assert_eq!(controls, vec![first_sha, second_sha]);
}

#[test]
fn a_same_context_check_from_another_workflow_is_ignored() {
    use crate::app::merge_steward_cmd::shadow_ledger::{self, Episode};
    let key = "generous-corp/pulp@main";
    let controls = BTreeMap::from([(CONTEXT.to_owned(), "version-skill-check.yml".to_owned())]);
    let firing = BTreeMap::from([((CONTEXT.to_owned(), "streak".to_owned()), 2)]);
    let base_sha = "4".repeat(40);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let line = argv.join(" ");
        if line.ends_with("--jq .sha") {
            return Ok(base_sha.clone());
        }
        if line.contains("/dispatches") {
            return Ok(r#"{"workflow_run_id":42}"#.to_owned());
        }
        if line.contains("/check-runs?") {
            return Ok(
                r#"{"check_runs":[
                    {"status":"completed","conclusion":"failure","started_at":"2026-11-12T23:00:00Z","check_suite":{"workflow_run":{"id":99}}},
                    {"status":"completed","conclusion":"success","started_at":"2026-11-12T23:01:00Z","check_suite":{"workflow_run":{"id":42}}}
                ]}"#
                .to_owned(),
            );
        }
        Err(format!("unexpected: {line}"))
    };
    let mut open: Vec<Episode> = Vec::new();
    let none = BTreeSet::new();
    shadow_ledger::advance(&mut open, key, &firing, &none, &none, at(24, 0));
    shadow_ledger::label(&mut open, key, REPO, "main", &controls, &run_gh, at(24, 0));
    assert_eq!(open[0].corroborated, Some(false));
}

#[test]
fn in_shadow_the_root_cause_route_only_reports() {
    let health = detect(&groups_at_0030(), &required(), at(24, 30));
    let dry = poison(health, PoisonHold::DryRun, &[]);
    assert!(dry.fixes.is_empty());
    let lines = base_poison::would_fast_track(&dry);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("would open or find the tracking issue"),
        "{lines:?}"
    );
}

#[test]
fn a_paused_head_is_not_readmitted_until_a_group_passes_its_context() {
    let first = open_pr(12028, 'a', Some(1), None);
    let mut ledger = StewardLedger::default();
    poison_pause::record(
        &mut ledger,
        &observation(vec![first.clone()]),
        &first,
        1,
        CONTEXT,
        at(24, 31),
    );
    let mut waiting = first;
    waiting.fact.queue_position = None;
    let observed = observation(vec![waiting.clone()]);
    let admit = StewardDecision::ArmMergeQueue;
    // Stale streak and an empty sample both read Clear but pass nothing:
    // normal admission must not refill the still-broken base.
    for detail in ["`x` streak is stale", "no recent merge group"] {
        let health = MergeGroupHealth::Clear {
            detail: detail.to_owned(),
            passed: Vec::new(),
            passed_by: std::collections::BTreeMap::new(),
        };
        poison_pause::reconcile(
            &mut ledger,
            &observed,
            Some(&poison(health, PoisonHold::On, &observed.prs)),
            at(27, 0),
        );
        let report =
            poison_pause::waiting_report(&ledger, &observed, &waiting, &admit).expect("held");
        assert_eq!(
            report.mutation.as_deref(),
            Some(format!("paused_awaiting_pass:{CONTEXT}").as_str())
        );
    }
    // A group passing an unrelated context is not a pass for this one.
    let other = MergeGroupHealth::Clear {
        detail: "clear".to_owned(),
        passed: vec!["macos".to_owned()],
        passed_by: std::collections::BTreeMap::new(),
    };
    poison_pause::reconcile(
        &mut ledger,
        &observed,
        Some(&poison(other, PoisonHold::On, &observed.prs)),
        at(27, 5),
    );
    assert!(poison_pause::waiting_report(&ledger, &observed, &waiting, &admit).is_some());
    // The context passes: the entry is released, first in line.
    let passed = MergeGroupHealth::Clear {
        detail: "clear".to_owned(),
        passed: vec![CONTEXT.to_owned()],
        passed_by: std::collections::BTreeMap::new(),
    };
    poison_pause::reconcile(
        &mut ledger,
        &observed,
        Some(&poison(passed, PoisonHold::On, &observed.prs)),
        at(27, 10),
    );
    assert!(poison_pause::waiting_report(&ledger, &observed, &waiting, &admit).is_none());
    assert_eq!(
        poison_pause::restore_rank(&ledger, REPO, "main", 12028),
        Some(0)
    );
}

#[test]
fn a_head_that_rode_a_failing_group_is_never_a_candidate() {
    let mut health = detect(&groups_at_0030(), &required(), at(24, 30));
    if let MergeGroupHealth::Poisoned { contexts } = &mut health {
        contexts[0].latest_failure_at = Some(at(24, 28));
    }
    // #11890 rode the 00:18Z failing group; its head later passes.
    let rode = open_pr(11890, 'f', None, Some(("SUCCESS", "2026-11-11T01:40:00Z")));
    let red = open_pr(12013, 'c', None, Some(("FAILURE", "2026-11-11T00:40:00Z")));
    let red_b = open_pr(12014, 'd', None, Some(("FAILURE", "2026-11-11T00:50:00Z")));
    let (_, candidates) =
        base_poison::assemble_fixes(BTreeMap::new(), &[rode, red, red_b], &health);
    assert!(candidates.is_empty(), "{candidates:?}");
}
