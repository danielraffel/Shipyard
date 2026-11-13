//! Fix-first through the steward's own decisions, on the 2026-11-13 Vellum
//! episode: `provenance-verify` red on the base tip from 10:32Z, #408 green
//! only before the break, #409 green after it and approved at 10:41Z.
use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::app::merge_steward_cmd::base_poison::{self, Admission, GreenHead, RepoPoison};
use crate::app::merge_steward_cmd::poison_pause::{self, Verdict};
use crate::base_health::fix_first::{TipReading, parse_tip};
use crate::base_health::merge_group::{GroupRef, MergeGroupHealth, PoisonedContext};
use crate::base_health::{AutoJump, BasePoisonPolicy, PoisonHold};

const CONTEXT: &str = "provenance-verify";
const APPROVAL: &str = "review-verdict";

/// An open pull request: its head's run of the context and its approval.
fn open_pr(
    number: u64,
    head: char,
    position: Option<u64>,
    green_at: Option<&str>,
    approved_at: Option<&str>,
) -> ObservedPr {
    let mut rollup = vec![serde_json::json!({
        "__typename": "CheckRun", "name": "macos", "status": "COMPLETED",
        "conclusion": "SUCCESS", "completedAt": "2026-11-13T09:00:00Z"
    })];
    if let Some(green_at) = green_at {
        rollup.push(serde_json::json!({
            "__typename": "CheckRun", "name": CONTEXT, "status": "COMPLETED",
            "conclusion": "SUCCESS", "completedAt": green_at
        }));
    }
    if let Some(approved_at) = approved_at {
        rollup.push(serde_json::json!({
            "__typename": "StatusContext", "context": APPROVAL, "state": "SUCCESS",
            "createdAt": approved_at
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

/// The three landed heads whose newest `provenance-verify` failed: #403,
/// #404 and #405.
fn streak() -> MergeGroupHealth {
    MergeGroupHealth::Poisoned {
        contexts: vec![PoisonedContext {
            context: CONTEXT.to_owned(),
            first_error: None,
            groups: [405_u64, 404, 403]
                .iter()
                .enumerate()
                .map(|(index, pr)| GroupRef {
                    head_sha: index.to_string().repeat(40),
                    prs: vec![*pr],
                })
                .collect(),
            latest_failure_at: None,
            check_id: None,
            same_first_error: None,
        }],
    }
}

/// The tip's `provenance-verify` runs as GitHub returns them: passed at
/// 09:53, failed at 10:32.
fn tip_red() -> TipReading {
    parse_tip(
        &serde_json::json!({"check_runs": [
            {"id": 131_829_205_683_u64, "name": CONTEXT, "status": "completed",
             "conclusion": "failure", "completed_at": "2026-11-13T10:32:36Z",
             "head_sha": "8c606bc733cb0104495158fdab5eb8e63b48c850"},
            {"id": 131_813_536_030_u64, "name": CONTEXT, "status": "completed",
             "conclusion": "success", "completed_at": "2026-11-13T09:53:01Z",
             "head_sha": "8c606bc733cb0104495158fdab5eb8e63b48c850"},
        ]}),
        CONTEXT,
        None,
    )
}

fn tip_green() -> TipReading {
    TipReading {
        sha: Some("f".repeat(40)),
        ..TipReading::default()
    }
}

fn poison(
    hold: PoisonHold,
    prs: &[ObservedPr],
    health: MergeGroupHealth,
    tip: TipReading,
) -> RepoPoison {
    let tip = BTreeMap::from([(CONTEXT.to_owned(), tip)]);
    let mut named = BTreeMap::new();
    let green = base_poison::green_head_fix(prs, &[], APPROVAL, &health, &tip);
    if let GreenHead::One(pr, evidence) = &green {
        named.insert(*pr, vec![evidence.clone()]);
    }
    let (fixes, mut candidates) = base_poison::assemble_fixes(named, prs, &health);
    let fix_ambiguity = match green {
        GreenHead::Ambiguous(prs) => {
            candidates.extend(prs.iter().copied());
            Some(base_poison::ambiguity_line(&prs))
        }
        _ => None,
    };
    RepoPoison {
        policy: BasePoisonPolicy {
            hold,
            control_workflows: BTreeMap::new(),
            pause: hold,
            auto_jump: AutoJump::On,
            read_error: None,
        },
        health,
        fixes,
        candidates,
        fix_read_error: None,
        fix_ambiguity,
        tip,
    }
}

fn pr_408() -> ObservedPr {
    open_pr(
        408,
        'a',
        None,
        Some("2026-11-13T09:58:37Z"),
        Some("2026-11-13T10:34:36Z"),
    )
}

fn pr_409() -> ObservedPr {
    open_pr(
        409,
        'b',
        None,
        Some("2026-11-13T10:35:37Z"),
        Some("2026-11-13T10:41:08Z"),
    )
}

#[test]
fn the_fix_is_the_approved_head_that_passed_the_context_after_the_tip_went_red() {
    let prs = [pr_408(), pr_409()];
    let tip = BTreeMap::from([(CONTEXT.to_owned(), tip_red())]);
    let GreenHead::One(pr, evidence) =
        base_poison::green_head_fix(&prs, &[], APPROVAL, &streak(), &tip)
    else {
        panic!("a green head is named");
    };
    assert_eq!(pr, 409);
    assert!(
        evidence.starts_with("green head: `provenance-verify` passed"),
        "{evidence}"
    );
}

#[test]
fn the_fix_is_jumped_and_never_held_while_the_other_approved_head_is_held() {
    let prs = [pr_408(), pr_409()];
    let on = poison(PoisonHold::On, &prs, streak(), tip_red());
    assert_eq!(on.fixes.keys().copied().collect::<Vec<_>>(), vec![409]);
    assert_eq!(
        base_poison::admission(Some(&on), 409),
        Admission::FastTrack { jump: true }
    );
    assert_eq!(
        base_poison::admission(Some(&on), 408),
        Admission::Hold {
            contexts: CONTEXT.to_owned()
        },
        "#408 passed only before the break: held, not jumped"
    );
    // Queued, the fix keeps its place; the other entry is paused.
    let queued_fix = open_pr(
        409,
        'b',
        Some(2),
        Some("2026-11-13T10:35:37Z"),
        Some("2026-11-13T10:41:08Z"),
    );
    let queued_other = open_pr(
        408,
        'a',
        Some(1),
        Some("2026-11-13T09:58:37Z"),
        Some("2026-11-13T10:34:36Z"),
    );
    let queued = |position| StewardDecision::Queued { position };
    assert_eq!(
        poison_pause::verdict(Some(&on), &queued_fix.fact, &queued(2)),
        Verdict::Keep
    );
    assert_eq!(
        poison_pause::verdict(Some(&on), &queued_other.fact, &queued(1)),
        Verdict::Pause {
            contexts: CONTEXT.to_owned(),
            position: 1
        }
    );
}

#[test]
fn a_streak_the_tip_does_not_confirm_holds_and_jumps_nothing() {
    let prs = [pr_408(), pr_409()];
    let unconfirmed = poison(PoisonHold::On, &prs, streak(), tip_green());
    assert!(
        unconfirmed.fixes.is_empty(),
        "no red tip, no green-head fix"
    );
    assert_eq!(
        base_poison::admission(Some(&unconfirmed), 408),
        Admission::Proceed
    );
    let unread = poison(
        PoisonHold::On,
        &prs,
        streak(),
        TipReading {
            error: Some("HTTP 502".to_owned()),
            ..TipReading::default()
        },
    );
    assert_eq!(
        base_poison::admission(Some(&unread), 408),
        Admission::Proceed,
        "an unread tip never holds"
    );
    let queued = open_pr(408, 'a', Some(1), None, Some("2026-11-13T10:34:36Z"));
    assert_eq!(
        poison_pause::verdict(
            Some(&unconfirmed),
            &queued.fact,
            &StewardDecision::Queued { position: 1 }
        ),
        Verdict::WouldPause {
            contexts: CONTEXT.to_owned()
        },
        "a streak alone only reports"
    );
}

#[test]
fn a_red_tip_without_the_streak_does_nothing() {
    let prs = [pr_408(), pr_409()];
    let clear = MergeGroupHealth::Clear {
        detail: "one failing group".to_owned(),
        passed: Vec::new(),
        passed_by: BTreeMap::new(),
    };
    let single = poison(PoisonHold::On, &prs, clear, tip_red());
    assert!(single.fixes.is_empty());
    assert_eq!(
        base_poison::admission(Some(&single), 408),
        Admission::Proceed
    );
    assert_eq!(
        base_poison::admission(Some(&single), 409),
        Admission::Proceed
    );
}

#[test]
fn the_hold_lifts_and_paused_entries_restore_when_the_tip_goes_green() {
    let prs = [pr_408(), pr_409()];
    let on = poison(PoisonHold::On, &prs, streak(), tip_red());
    let paused = open_pr(408, 'a', None, None, Some("2026-11-13T10:34:36Z"));
    let mut observation = observation_for(paused.clone(), true);
    observation.prs = vec![paused.clone()];
    let mut ledger = StewardLedger::default();
    poison_pause::record(&mut ledger, &observation, &paused, 1, CONTEXT, Utc::now());
    poison_pause::reconcile(&mut ledger, &observation, Some(&on), Utc::now());
    assert_eq!(
        poison_pause::restore_rank(&ledger, &observation.repo, &observation.base, 408),
        None,
        "still red: waits"
    );
    // The tip goes green while the older groups still make up a streak.
    let green = poison(PoisonHold::On, &prs, streak(), tip_green());
    assert_eq!(
        base_poison::admission(Some(&green), 408),
        Admission::Proceed
    );
    poison_pause::reconcile(&mut ledger, &observation, Some(&green), Utc::now());
    assert_eq!(
        poison_pause::restore_rank(&ledger, &observation.repo, &observation.base, 408),
        Some(0),
        "restored first"
    );
}

#[test]
fn dry_run_holds_nothing_and_jumps_nothing() {
    let prs = [pr_408(), pr_409()];
    let dry = poison(PoisonHold::DryRun, &prs, streak(), tip_red());
    assert_eq!(
        base_poison::admission(Some(&dry), 409),
        Admission::FastTrack { jump: false }
    );
    assert_eq!(
        base_poison::admission(Some(&dry), 408),
        Admission::WouldHold {
            contexts: CONTEXT.to_owned()
        }
    );
    let queued = open_pr(408, 'a', Some(1), None, Some("2026-11-13T10:34:36Z"));
    assert_eq!(
        poison_pause::verdict(
            Some(&dry),
            &queued.fact,
            &StewardDecision::Queued { position: 1 }
        ),
        Verdict::WouldPause {
            contexts: CONTEXT.to_owned()
        }
    );
    let lines = base_poison::would_fast_track(&dry);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("#409 would fast-track"), "{lines:?}");
}

/// The shadow ledger keeps a `tip_red` episode with the pick, and once a
/// merge group passes the context it records that group's pull requests,
/// so the pick is scored against the actual repair.
#[test]
fn the_shadow_ledger_scores_the_pick_against_the_passing_group() {
    use crate::app::merge_steward_cmd::shadow_ledger;
    let prs = [pr_408(), pr_409()];
    let on = poison(PoisonHold::On, &prs, streak(), tip_red());
    let key = "generous-corp/vellum@main";
    let mut open = Vec::new();
    let now = Utc::now();
    let firing = shadow_ledger::firing(&on);
    assert!(firing.contains_key(&(CONTEXT.to_owned(), "tip_red".to_owned())));
    shadow_ledger::advance(
        &mut open,
        key,
        &firing,
        &BTreeSet::new(),
        &BTreeSet::from([408]),
        now,
    );
    shadow_ledger::note_fix_first(&mut open, key, Some(409), &BTreeMap::new());
    shadow_ledger::advance(
        &mut open,
        key,
        &BTreeMap::new(),
        &BTreeSet::new(),
        &BTreeSet::new(),
        now,
    );
    shadow_ledger::note_fix_first(
        &mut open,
        key,
        None,
        &BTreeMap::from([(CONTEXT.to_owned(), vec![409])]),
    );
    let ended = shadow_ledger::settle(&mut open, key);
    let tip_red = ended
        .iter()
        .find(|episode| episode.rule == "tip_red")
        .expect("tip_red episode");
    assert_eq!(tip_red.fix_picks, BTreeSet::from([409]));
    assert_eq!(tip_red.fixed_by, vec![409]);
    let score = shadow_ledger::fix_first_score(&ended);
    assert_eq!(score["pick_repaired"], 1);
    assert_eq!(score["pick_missed"], 0);
}

/// Two approved heads both pass the context after the break and nothing
/// singles one out: neither is jumped, neither is held, the refusal is
/// reported, and the other approved head is still held.
#[test]
fn two_green_heads_are_ambiguous_neither_jumped_nor_held() {
    let rival = open_pr(
        410,
        'c',
        None,
        Some("2026-11-13T10:36:00Z"),
        Some("2026-11-13T10:42:00Z"),
    );
    let prs = [pr_408(), pr_409(), rival];
    let on = poison(PoisonHold::On, &prs, streak(), tip_red());
    assert!(on.fixes.is_empty(), "{:?}", on.fixes);
    assert!(
        on.fix_ambiguity
            .as_deref()
            .is_some_and(|line| line.starts_with("fix ambiguous: #409, #410")),
        "{:?}",
        on.fix_ambiguity
    );
    assert_eq!(base_poison::admission(Some(&on), 409), Admission::Proceed);
    assert_eq!(base_poison::admission(Some(&on), 410), Admission::Proceed);
    assert!(matches!(
        base_poison::admission(Some(&on), 408),
        Admission::Hold { .. }
    ));
    let queued = open_pr(
        409,
        'b',
        Some(1),
        Some("2026-11-13T10:35:37Z"),
        Some("2026-11-13T10:41:08Z"),
    );
    assert_eq!(
        poison_pause::verdict(
            Some(&on),
            &queued.fact,
            &StewardDecision::Queued { position: 1 }
        ),
        Verdict::Keep,
        "an ambiguous candidate is never paused"
    );
}

/// A tip that could not be read confirms nothing, and the pass says its
/// reading is partial rather than clean; the next pass reads it again.
#[test]
fn an_unread_tip_is_reported_partial_and_retried() {
    let calls = std::cell::Cell::new(0);
    let failing = |_argv: &[String]| -> Result<String, String> {
        calls.set(calls.get() + 1);
        Err("HTTP 502".to_owned())
    };
    let reading = base_poison::read_tip(&failing, "o/r", "main", CONTEXT, None);
    assert!(!reading.red);
    assert_eq!(reading.error.as_deref(), Some("HTTP 502"));
    let prs = [pr_408(), pr_409()];
    let unread = poison(PoisonHold::On, &prs, streak(), reading);
    assert_eq!(
        base_poison::admission(Some(&unread), 408),
        Admission::Proceed
    );
    assert!(unread.fixes.is_empty());
    assert_eq!(
        unread.partial().as_deref(),
        Some("tip `provenance-verify`: HTTP 502")
    );
    let _ = base_poison::read_tip(&failing, "o/r", "main", CONTEXT, None);
    assert_eq!(calls.get(), 2, "nothing is cached: each pass reads again");
    let clean = poison(PoisonHold::On, &prs, streak(), tip_red());
    assert_eq!(clean.partial(), None);
}

/// An episode opened on a pass whose tip read failed is recorded as
/// partial in the shadow ledger, never as a clean episode.
#[test]
fn the_shadow_ledger_marks_an_episode_built_on_an_unread_tip_partial() {
    use crate::app::merge_steward_cmd::shadow_ledger;
    let dir = tempfile::tempdir().expect("tempdir");
    let prs = [pr_408(), pr_409()];
    let unread = poison(
        PoisonHold::DryRun,
        &prs,
        streak(),
        TipReading {
            error: Some("HTTP 502".to_owned()),
            ..TipReading::default()
        },
    );
    let no_call = |argv: &[String]| -> Result<String, String> {
        Err(format!("no call expected: {}", argv.join(" ")))
    };
    let report = shadow_ledger::record_pass(
        dir.path(),
        "Generous-Corp/vellum",
        "main",
        Some(&unread),
        &BTreeSet::new(),
        &BTreeSet::new(),
        &no_call,
        Utc::now(),
    );
    let open = report["open"].as_array().expect("open episodes");
    assert!(!open.is_empty());
    assert!(
        open.iter().all(|episode| episode["partial"] == true),
        "{report}"
    );
}
