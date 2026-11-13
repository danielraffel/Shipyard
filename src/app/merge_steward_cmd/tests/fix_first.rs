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
    let (green, covering) = base_poison::green_head_fix(
        prs,
        &[],
        APPROVAL,
        &health,
        &base_poison::confirmed_red_since(&health, &tip, &BTreeMap::new()),
    );
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
            acts: crate::base_health::TriageActs::default(),
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
        red_everywhere: std::collections::BTreeMap::new(),
        tip,
        covering,
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
    let GreenHead::One(pr, evidence) = base_poison::green_head_fix(
        &prs,
        &[],
        APPROVAL,
        &streak(),
        &base_poison::confirmed_red_since(&streak(), &tip, &BTreeMap::new()),
    )
    .0
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

fn with_acts(mut poison: RepoPoison, mode: PoisonHold) -> RepoPoison {
    poison.policy.acts = crate::base_health::TriageActs::all(mode);
    poison
}

/// #409 was enqueued behind #408 before the episode was confirmed: the
/// steward moves it to the front, bound to its head, once per episode.
#[test]
fn a_queued_fix_behind_other_entries_is_reordered_once_per_episode() {
    use crate::app::merge_steward_cmd::fix_reorder;
    let fix = open_pr(
        409,
        'b',
        Some(2),
        Some("2026-11-13T10:35:37Z"),
        Some("2026-11-13T10:41:08Z"),
    );
    let other = open_pr(
        408,
        'a',
        Some(1),
        Some("2026-11-13T09:58:37Z"),
        Some("2026-11-13T10:34:36Z"),
    );
    let prs = [other, fix];
    let on = with_acts(
        poison(PoisonHold::On, &prs, streak(), tip_red()),
        PoisonHold::On,
    );
    let mut observation = observation_for(prs[0].clone(), true);
    observation.prs = prs.to_vec();
    let candidate = fix_reorder::candidate(&on, &observation)
        .expect("no refusal")
        .expect("a candidate");
    assert_eq!((candidate.pr, candidate.position), (409, 2));
    assert_eq!(candidate.head, "b".repeat(40), "bound to the census head");
    let dir = tempfile::tempdir().expect("tempdir");
    let now = Utc::now();
    let calls = std::cell::Cell::new(0);
    // A failed attempt is recorded as failed and retried by the next pass.
    let line = fix_reorder::decide(
        dir.path(),
        &observation.repo,
        PoisonHold::On,
        &candidate,
        now,
        || {
            calls.set(calls.get() + 1);
            "failed: dequeue: HTTP 502".to_owned()
        },
    );
    assert!(line.contains("failed: dequeue"), "{line}");
    let line = fix_reorder::decide(
        dir.path(),
        &observation.repo,
        PoisonHold::On,
        &candidate,
        now,
        || {
            calls.set(calls.get() + 1);
            "jumped".to_owned()
        },
    );
    assert!(line.ends_with("jumped"), "{line}");
    let line = fix_reorder::decide(
        dir.path(),
        &observation.repo,
        PoisonHold::On,
        &candidate,
        now,
        || {
            calls.set(calls.get() + 1);
            "jumped".to_owned()
        },
    );
    assert!(line.contains("already decided"), "{line}");
    assert_eq!(
        calls.get(),
        2,
        "one retry after the failure, then never again"
    );
    assert_eq!(fix_reorder::load(dir.path()).len(), 2);
}

#[test]
fn a_fix_at_the_front_or_unqueued_or_unconfirmed_is_never_reordered() {
    use crate::app::merge_steward_cmd::fix_reorder;
    let front = open_pr(
        409,
        'b',
        Some(1),
        Some("2026-11-13T10:35:37Z"),
        Some("2026-11-13T10:41:08Z"),
    );
    let prs = [pr_408(), front];
    let on = with_acts(
        poison(PoisonHold::On, &prs, streak(), tip_red()),
        PoisonHold::On,
    );
    let mut observation = observation_for(prs[0].clone(), true);
    observation.prs = prs.to_vec();
    let refused = fix_reorder::candidate(&on, &observation).expect_err("position 1");
    assert!(refused.contains("already first"), "{refused}");
    let unqueued = [pr_408(), pr_409()];
    let on = with_acts(
        poison(PoisonHold::On, &unqueued, streak(), tip_red()),
        PoisonHold::On,
    );
    observation.prs = unqueued.to_vec();
    assert_eq!(
        fix_reorder::candidate(&on, &observation),
        Ok(None),
        "an unqueued fix is jumped by the enqueue itself"
    );
    let queued = [
        pr_408(),
        open_pr(
            409,
            'b',
            Some(3),
            Some("2026-11-13T10:35:37Z"),
            Some("2026-11-13T10:41:08Z"),
        ),
    ];
    observation.prs = queued.to_vec();
    let unconfirmed = with_acts(
        poison(PoisonHold::On, &queued, streak(), tip_green()),
        PoisonHold::On,
    );
    assert_eq!(fix_reorder::candidate(&unconfirmed, &observation), Ok(None));
    let off = with_acts(
        poison(PoisonHold::On, &queued, streak(), tip_red()),
        PoisonHold::Off,
    );
    assert_eq!(fix_reorder::candidate(&off, &observation), Ok(None));
}

#[test]
fn a_dry_run_reorder_records_would_reorder_and_never_acts() {
    use crate::app::merge_steward_cmd::fix_reorder;
    let queued = [
        pr_408(),
        open_pr(
            409,
            'b',
            Some(2),
            Some("2026-11-13T10:35:37Z"),
            Some("2026-11-13T10:41:08Z"),
        ),
    ];
    let dry = with_acts(
        poison(PoisonHold::On, &queued, streak(), tip_red()),
        PoisonHold::DryRun,
    );
    let mut observation = observation_for(queued[0].clone(), true);
    observation.prs = queued.to_vec();
    let candidate = fix_reorder::candidate(&dry, &observation)
        .expect("ok")
        .expect("candidate");
    let dir = tempfile::tempdir().expect("tempdir");
    let line = fix_reorder::decide(
        dir.path(),
        &observation.repo,
        PoisonHold::DryRun,
        &candidate,
        Utc::now(),
        || panic!("dry-run never acts"),
    );
    assert!(line.contains("would_reorder from position 2"), "{line}");
}

fn run(
    id: u64,
    event: &str,
    branch: &str,
    head: char,
    status: &str,
) -> crate::merge_steward::StewardRun {
    crate::merge_steward::StewardRun {
        id,
        workflow_id: 1,
        run_attempt: 1,
        workflow: "macOS GPU".to_owned(),
        head_sha: head.to_string().repeat(40),
        head_branch: branch.to_owned(),
        status: status.to_owned(),
        event: event.to_owned(),
        pull_request_number: None,
        created_at: "2026-11-13T10:50:00Z".to_owned(),
        jobs: Vec::new(),
    }
}

struct FakeActs {
    refs: BTreeMap<String, Result<String, String>>,
    cancel: Result<(), String>,
    reads: std::cell::RefCell<Vec<String>>,
    cancels: std::cell::RefCell<Vec<u64>>,
}

impl crate::app::merge_steward_cmd::orphan_groups::Acts for FakeActs {
    fn read_ref(&self, branch: &str) -> Result<String, String> {
        self.reads.borrow_mut().push(branch.to_owned());
        self.refs
            .get(branch)
            .cloned()
            .unwrap_or_else(|| Err("gh: Not Found (HTTP 404)".to_owned()))
    }
    fn cancel(&self, run: &crate::merge_steward::StewardRun) -> Result<(), String> {
        self.cancels.borrow_mut().push(run.id);
        self.cancel.clone()
    }
}

/// Vellum, 11:02Z: the reorder rebuilt the queue; the run on
/// `pr-408-8c606bc7` kept going. Only that run is cancelled: the live
/// group, a push run and a `pull_request` run are never touched, and a ref
/// that cannot be read leaves its run alone.
#[test]
fn only_a_merge_group_run_whose_queue_branch_is_confirmed_gone_is_cancelled() {
    use crate::app::merge_steward_cmd::orphan_groups;
    let orphan = "gh-readonly-queue/main/pr-408-8c606bc733cb0104495158fdab5eb8e63b48c850";
    let live = "gh-readonly-queue/main/pr-409-8c606bc733cb0104495158fdab5eb8e63b48c850";
    let flaky = "gh-readonly-queue/main/pr-410-8c606bc733cb0104495158fdab5eb8e63b48c850";
    let mut observation = observation_for(pr_408(), true);
    observation.merge_group_heads = BTreeMap::from([(409, "e".repeat(40))]);
    observation.runs = vec![
        run(43_410_542_522, "merge_group", orphan, 'd', "in_progress"),
        run(2, "merge_group", live, 'e', "in_progress"),
        run(3, "push", "main", 'f', "in_progress"),
        run(4, "pull_request", "branch-408", 'a', "in_progress"),
        run(5, "merge_group", flaky, 'c', "queued"),
        run(6, "merge_group", orphan, 'd', "completed"),
    ];
    let acts = FakeActs {
        refs: BTreeMap::from([(flaky.to_owned(), Err("HTTP 502".to_owned()))]),
        cancel: Ok(()),
        reads: std::cell::RefCell::default(),
        cancels: std::cell::RefCell::default(),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let lines = orphan_groups::decide(
        PoisonHold::On,
        Some(dir.path()),
        &observation,
        &acts,
        Utc::now(),
    );
    assert_eq!(*acts.cancels.borrow(), vec![43_410_542_522]);
    assert_eq!(
        acts.reads.borrow().len(),
        2,
        "one read per suspect branch only"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("43410542522") && line.ends_with("cancelled")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("ref unread, left running")),
        "{lines:?}"
    );
    // Recorded: the next pass does not cancel it again.
    let again = orphan_groups::decide(
        PoisonHold::On,
        Some(dir.path()),
        &observation,
        &acts,
        Utc::now(),
    );
    assert_eq!(acts.cancels.borrow().len(), 1);
    assert!(
        again.iter().any(|line| line.contains("already cancelled")),
        "{again:?}"
    );
}

#[test]
fn a_failed_orphan_cancel_is_retried_and_dry_run_never_cancels() {
    use crate::app::merge_steward_cmd::orphan_groups;
    let orphan = "gh-readonly-queue/main/pr-408-8c606bc733cb0104495158fdab5eb8e63b48c850";
    let mut observation = observation_for(pr_408(), true);
    observation.runs = vec![run(7, "merge_group", orphan, 'd', "in_progress")];
    let failing = FakeActs {
        refs: BTreeMap::new(),
        cancel: Err("HTTP 500".to_owned()),
        reads: std::cell::RefCell::default(),
        cancels: std::cell::RefCell::default(),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let first = orphan_groups::decide(
        PoisonHold::On,
        Some(dir.path()),
        &observation,
        &failing,
        Utc::now(),
    );
    assert!(first[0].ends_with("failed: HTTP 500"), "{first:?}");
    let _ = orphan_groups::decide(
        PoisonHold::On,
        Some(dir.path()),
        &observation,
        &failing,
        Utc::now(),
    );
    assert_eq!(
        failing.cancels.borrow().len(),
        2,
        "a failed cancel is retried"
    );
    let dry = FakeActs {
        refs: BTreeMap::new(),
        cancel: Ok(()),
        reads: std::cell::RefCell::default(),
        cancels: std::cell::RefCell::default(),
    };
    let lines = orphan_groups::decide(
        PoisonHold::DryRun,
        Some(dir.path()),
        &observation,
        &dry,
        Utc::now(),
    );
    assert!(dry.cancels.borrow().is_empty());
    assert!(lines[0].contains("would cancel run 7"), "{lines:?}");
    assert!(
        orphan_groups::decide(
            PoisonHold::Off,
            Some(dir.path()),
            &observation,
            &dry,
            Utc::now()
        )
        .is_empty()
    );
}

/// The streak's groups carried #405, #404 and #403 (see [`streak`]); #404
/// is still open and unqueued at head 'a' after its ejection.
fn ejected_404() -> ObservedPr {
    open_pr(404, 'a', None, None, Some("2026-11-13T09:30:00Z"))
}

fn removals(head: &str, at: &[(&str, &str)]) -> serde_json::Value {
    let nodes = at
        .iter()
        .map(|(reason, created)| {
            serde_json::json!({
                "__typename": "RemovedFromMergeQueueEvent",
                "reason": reason,
                "createdAt": created,
                "beforeCommit": {"oid": head},
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({"data": {"repository": {"pullRequest": {"timelineItems": {"nodes": nodes}}}}})
}

fn removal_count(queue: &serde_json::Value) -> usize {
    queue
        .pointer("/data/repository/pullRequest/timelineItems/nodes")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len)
}

#[test]
#[allow(clippy::too_many_lines)] // One episode, read from record to re-enqueue.
fn a_red_base_ejection_is_excused_once_the_base_is_green_and_only_for_that_head() {
    use crate::app::merge_steward_cmd::red_base_ejections;
    let prs = [ejected_404(), pr_409()];
    let mut observation = observation_for(prs[0].clone(), true);
    observation.prs = prs.to_vec();
    let red = poison(PoisonHold::On, &prs, streak(), tip_red());
    let green = poison(PoisonHold::On, &prs, streak(), tip_green());
    let mut ledger = StewardLedger::default();
    let at = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text)
            .expect("time")
            .with_timezone(&Utc)
    };
    // While red: recorded, nothing excused.
    let lines = red_base_ejections::reconcile(
        PoisonHold::On,
        true,
        &mut ledger,
        &observation,
        Some(&red),
        at("2026-11-13T10:40:00Z"),
    );
    assert!(lines.is_empty(), "{lines:?}");
    let entries = red_base_ejections::entries(&ledger, &observation.repo, &observation.base);
    assert_eq!(entries.len(), 1);
    assert_eq!((entries[0].pr, entries[0].ready), (404, false));
    let head = "a".repeat(40);
    let queue = removals(
        &head,
        &[
            ("FAILED_CHECKS", "2026-11-13T10:20:00Z"),
            ("failed_checks", "2026-11-13T10:38:00Z"),
        ],
    );
    let before = red_base_ejections::excused_queue(
        &ledger,
        &observation.repo,
        &observation.base,
        404,
        &head,
        &queue,
    );
    assert_eq!(removal_count(&before), 2, "nothing excused while red");
    // An unread tip does not end the episode.
    let unread = poison(
        PoisonHold::On,
        &prs,
        streak(),
        TipReading {
            error: Some("HTTP 502".to_owned()),
            ..TipReading::default()
        },
    );
    red_base_ejections::reconcile(
        PoisonHold::On,
        true,
        &mut ledger,
        &observation,
        Some(&unread),
        at("2026-11-13T11:00:00Z"),
    );
    assert!(!red_base_ejections::entries(&ledger, &observation.repo, &observation.base)[0].ready);
    // Green: the window closes and the ejections inside it are excused.
    let lines = red_base_ejections::reconcile(
        PoisonHold::On,
        true,
        &mut ledger,
        &observation,
        Some(&green),
        at("2026-11-13T11:10:00Z"),
    );
    assert!(
        lines[0].contains("excused from the ejection cap"),
        "{lines:?}"
    );
    let after = red_base_ejections::excused_queue(
        &ledger,
        &observation.repo,
        &observation.base,
        404,
        &head,
        &queue,
    );
    assert_eq!(removal_count(&after), 0);
    // Another head of the same pull request is never excused.
    let moved = red_base_ejections::excused_queue(
        &ledger,
        &observation.repo,
        &observation.base,
        404,
        &"b".repeat(40),
        &removals(
            &"b".repeat(40),
            &[("failed_checks", "2026-11-13T10:38:00Z")],
        ),
    );
    assert_eq!(removal_count(&moved), 1);
    // A later ejection after the base went green is judged as usual.
    let later = removals(
        &head,
        &[
            ("failed_checks", "2026-11-13T10:38:00Z"),
            ("failed_checks", "2026-11-13T12:00:00Z"),
        ],
    );
    let kept = red_base_ejections::excused_queue(
        &ledger,
        &observation.repo,
        &observation.base,
        404,
        &head,
        &later,
    );
    assert_eq!(removal_count(&kept), 2);
    // Once queued again at that head, the entry is done: once per head.
    let requeued = open_pr(404, 'a', Some(1), None, Some("2026-11-13T09:30:00Z"));
    observation.prs = vec![requeued, pr_409()];
    let lines = red_base_ejections::reconcile(
        PoisonHold::On,
        true,
        &mut ledger,
        &observation,
        Some(&green),
        at("2026-11-13T11:20:00Z"),
    );
    assert!(lines[0].ends_with("re-enqueued"), "{lines:?}");
    assert!(red_base_ejections::entries(&ledger, &observation.repo, &observation.base).is_empty());
}

#[test]
fn a_dry_run_reenqueue_reports_and_excuses_nothing_and_a_moved_head_is_dropped() {
    use crate::app::merge_steward_cmd::red_base_ejections;
    let prs = [ejected_404(), pr_409()];
    let mut observation = observation_for(prs[0].clone(), true);
    observation.prs = prs.to_vec();
    let red = poison(PoisonHold::On, &prs, streak(), tip_red());
    let green = poison(PoisonHold::On, &prs, streak(), tip_green());
    let mut ledger = StewardLedger::default();
    red_base_ejections::reconcile(
        PoisonHold::DryRun,
        true,
        &mut ledger,
        &observation,
        Some(&red),
        Utc::now(),
    );
    let lines = red_base_ejections::reconcile(
        PoisonHold::DryRun,
        true,
        &mut ledger,
        &observation,
        Some(&green),
        Utc::now(),
    );
    assert!(lines[0].contains("would re-enqueue"), "{lines:?}");
    let head = "a".repeat(40);
    let queue = removals(&head, &[("failed_checks", &Utc::now().to_rfc3339())]);
    let same = red_base_ejections::excused_queue(
        &ledger,
        &observation.repo,
        &observation.base,
        404,
        &head,
        &queue,
    );
    assert_eq!(removal_count(&same), 1, "dry-run excuses nothing");
    observation.prs = vec![open_pr(404, 'c', None, None, None), pr_409()];
    let lines = red_base_ejections::reconcile(
        PoisonHold::DryRun,
        true,
        &mut ledger,
        &observation,
        Some(&green),
        Utc::now(),
    );
    assert!(lines[0].ends_with("head moved"), "{lines:?}");
    assert!(red_base_ejections::entries(&ledger, &observation.repo, &observation.base).is_empty());
    red_base_ejections::reconcile(
        PoisonHold::Off,
        true,
        &mut ledger,
        &observation,
        Some(&red),
        Utc::now(),
    );
    assert!(
        red_base_ejections::entries(&ledger, &observation.repo, &observation.base).is_empty(),
        "off records nothing"
    );
}

/// A red-everywhere episode on the context's check confirms the streak
/// over a stale tip, and the green-head fix is measured from when it began.
#[test]
fn red_everywhere_over_a_stale_tip_confirms_and_names_the_fix() {
    let prs = [pr_408(), pr_409()];
    let stale = TipReading {
        newest_at: Some(
            chrono::DateTime::parse_from_rfc3339("2026-11-13T09:53:01Z")
                .expect("time")
                .with_timezone(&Utc),
        ),
        ..TipReading::default()
    };
    let since = chrono::DateTime::parse_from_rfc3339("2026-11-13T10:30:00Z")
        .expect("time")
        .with_timezone(&Utc);
    let mut confirmed = poison(PoisonHold::On, &prs, streak(), stale.clone());
    confirmed.red_everywhere = BTreeMap::from([(CONTEXT.to_owned(), since)]);
    assert_eq!(confirmed.confirmed_contexts(), vec![CONTEXT.to_owned()]);
    let red_since = base_poison::confirmed_red_since(
        &confirmed.health,
        &confirmed.tip,
        &confirmed.red_everywhere,
    );
    let GreenHead::One(pr, _) =
        base_poison::green_head_fix(&prs, &[], APPROVAL, &streak(), &red_since).0
    else {
        panic!("a fix");
    };
    assert_eq!(pr, 409, "#408 passed before the episode began");
    // Without red everywhere the stale tip confirms nothing.
    let alone = poison(PoisonHold::On, &prs, streak(), stale);
    assert!(alone.confirmed_contexts().is_empty());
    // Red everywhere never confirms over a fresh green or an unread tip.
    let fresh = TipReading {
        newest_at: Some(since + chrono::Duration::minutes(5)),
        ..TipReading::default()
    };
    let unread = TipReading {
        error: Some("HTTP 502".to_owned()),
        ..TipReading::default()
    };
    for tip in [fresh, unread] {
        let mut not_confirmed = poison(PoisonHold::On, &prs, streak(), tip);
        not_confirmed.red_everywhere = BTreeMap::from([(CONTEXT.to_owned(), since)]);
        assert!(not_confirmed.confirmed_contexts().is_empty());
        assert!(matches!(
            base_poison::admission(Some(&not_confirmed), 408),
            Admission::Proceed
        ));
    }
}

const OTHER: &str = "macos-gpu";

/// An approved open pull request with the given contexts green at times.
fn pr_with_greens(number: u64, head: char, greens: &[(&str, &str)]) -> ObservedPr {
    let mut rollup = greens
        .iter()
        .map(|(context, at)| {
            serde_json::json!({
                "__typename": "CheckRun", "name": context, "status": "COMPLETED",
                "conclusion": "SUCCESS", "completedAt": at
            })
        })
        .collect::<Vec<_>>();
    rollup.push(serde_json::json!({
        "__typename": "StatusContext", "context": APPROVAL, "state": "SUCCESS",
        "createdAt": "2026-11-13T10:50:00Z"
    }));
    parse_pr(
        &serde_json::json!({
            "id": format!("PR_{number}"), "number": number, "state": "OPEN", "isDraft": false,
            "baseRefName": "main", "headRefOid": head.to_string().repeat(40),
            "headRefName": format!("branch-{number}"), "mergeStateStatus": "CLEAN",
            "autoMergeRequest": null, "labels": [], "statusCheckRollup": rollup,
        }),
        &BTreeMap::new(),
    )
    .expect("PR")
}

/// Both contexts fail the same streak of groups.
fn two_context_streak() -> MergeGroupHealth {
    let MergeGroupHealth::Poisoned { contexts } = streak() else {
        unreachable!()
    };
    let mut other = contexts[0].clone();
    other.context = OTHER.to_owned();
    let mut both = contexts;
    both.push(other);
    MergeGroupHealth::Poisoned { contexts: both }
}

fn red_on_both() -> BTreeMap<String, TipReading> {
    [CONTEXT, OTHER]
        .iter()
        .map(|context| ((*context).to_owned(), tip_red()))
        .collect()
}

fn two_context_poison(prs: &[ObservedPr], tip: BTreeMap<String, TipReading>) -> RepoPoison {
    let health = two_context_streak();
    let (green, covering) = base_poison::green_head_fix(
        prs,
        &[],
        APPROVAL,
        &health,
        &base_poison::confirmed_red_since(&health, &tip, &BTreeMap::new()),
    );
    let mut named = BTreeMap::new();
    if let GreenHead::One(pr, evidence) = green {
        named.insert(pr, vec![evidence]);
    }
    let (fixes, candidates) = base_poison::assemble_fixes(named, prs, &health);
    RepoPoison {
        policy: BasePoisonPolicy {
            acts: crate::base_health::TriageActs::default(),
            hold: PoisonHold::On,
            control_workflows: BTreeMap::new(),
            pause: PoisonHold::On,
            auto_jump: AutoJump::On,
            read_error: None,
        },
        health,
        fixes,
        candidates,
        fix_read_error: None,
        fix_ambiguity: None,
        red_everywhere: BTreeMap::new(),
        tip,
        covering,
    }
}

/// With two contexts red, a head that passes only one of them is neither
/// the fix nor exempt: it would form a group that still fails the other.
#[test]
fn a_fix_must_pass_every_confirmed_context() {
    let half = pr_with_greens(500, 'c', &[(CONTEXT, "2026-11-13T10:40:00Z")]);
    let whole = pr_with_greens(
        501,
        'd',
        &[
            (CONTEXT, "2026-11-13T10:40:00Z"),
            (OTHER, "2026-11-13T10:41:00Z"),
        ],
    );
    let only_half = two_context_poison(std::slice::from_ref(&half), red_on_both());
    assert!(only_half.fixes.is_empty(), "{:?}", only_half.fixes);
    assert!(only_half.covering.is_empty());
    assert!(matches!(
        base_poison::admission(Some(&only_half), 500),
        Admission::Hold { .. }
    ));
    let both = two_context_poison(&[half, whole], red_on_both());
    assert_eq!(both.fixes.keys().copied().collect::<Vec<_>>(), vec![501]);
    let evidence = both.fixes[&501].join(" | ");
    assert!(
        evidence.contains(CONTEXT) && evidence.contains(OTHER),
        "evidence names both contexts: {evidence}"
    );
    assert_eq!(
        base_poison::admission(Some(&both), 501),
        Admission::FastTrack { jump: true }
    );
    assert!(matches!(
        base_poison::admission(Some(&both), 500),
        Admission::Hold { .. }
    ));
}

/// A named or measured exemption for one context never bypasses the hold
/// for another confirmed context.
#[test]
fn an_exemption_for_one_context_does_not_bypass_another() {
    let half = pr_with_greens(500, 'c', &[(CONTEXT, "2026-11-13T10:40:00Z")]);
    let mut poison = two_context_poison(std::slice::from_ref(&half), red_on_both());
    poison.candidates.insert(500);
    poison
        .fixes
        .insert(500, vec!["label `shipyard:unblock`".to_owned()]);
    assert!(matches!(
        base_poison::admission(Some(&poison), 500),
        Admission::Hold { .. }
    ));
}

/// A confirmed context whose red time is unknown exempts nobody.
#[test]
fn a_confirmed_context_without_a_red_time_exempts_nobody() {
    let whole = pr_with_greens(
        501,
        'd',
        &[
            (CONTEXT, "2026-11-13T10:40:00Z"),
            (OTHER, "2026-11-13T10:41:00Z"),
        ],
    );
    let mut tip = red_on_both();
    tip.insert(
        OTHER.to_owned(),
        TipReading {
            red: true,
            ..TipReading::default()
        },
    );
    let poison = two_context_poison(&[whole], tip);
    assert!(poison.covering.is_empty());
    assert!(poison.fixes.is_empty());
    assert!(matches!(
        base_poison::admission(Some(&poison), 501),
        Admission::Hold { .. }
    ));
}

/// A queued head named or measured for one red context is paused while
/// another context is also confirmed; only a head covering every confirmed
/// context keeps its place.
#[test]
fn the_pause_keeps_only_a_head_that_covers_every_confirmed_context() {
    let mut half = pr_with_greens(500, 'c', &[(CONTEXT, "2026-11-13T10:40:00Z")]);
    half.fact.queue_position = Some(1);
    let mut whole = pr_with_greens(
        501,
        'd',
        &[
            (CONTEXT, "2026-11-13T10:40:00Z"),
            (OTHER, "2026-11-13T10:41:00Z"),
        ],
    );
    whole.fact.queue_position = Some(2);
    let mut poison = two_context_poison(&[half.clone(), whole.clone()], red_on_both());
    poison.candidates.insert(500);
    poison
        .fixes
        .insert(500, vec!["label `shipyard:unblock`".to_owned()]);
    let queued = |position| StewardDecision::Queued { position };
    assert!(matches!(
        poison_pause::verdict(Some(&poison), &half.fact, &queued(1)),
        Verdict::Pause { .. }
    ));
    assert_eq!(
        poison_pause::verdict(Some(&poison), &whole.fact, &queued(2)),
        Verdict::Keep
    );
}

/// A check from another app never makes a head cover a context whose
/// required rule pins GitHub Actions.
#[test]
fn coverage_needs_the_required_app_identity() {
    let mut rollup_pr = pr_with_greens(502, 'e', &[(CONTEXT, "2026-11-13T10:40:00Z")]);
    for check in &mut rollup_pr.fact.checks {
        if check.name == CONTEXT {
            check.app_id = Some(999);
        }
    }
    let health = streak();
    let tip = BTreeMap::from([(CONTEXT.to_owned(), tip_red())]);
    let pinned = [crate::merge_steward::RequiredCheck {
        context: CONTEXT.to_owned(),
        app_id: Some(15_368),
    }];
    let (_, covering) = base_poison::green_head_fix(
        std::slice::from_ref(&rollup_pr),
        &pinned,
        APPROVAL,
        &health,
        &base_poison::confirmed_red_since(&health, &tip, &BTreeMap::new()),
    );
    assert!(covering.is_empty(), "{covering:?}");
    let (_, unpinned) = base_poison::green_head_fix(
        std::slice::from_ref(&rollup_pr),
        &[],
        APPROVAL,
        &health,
        &base_poison::confirmed_red_since(&health, &tip, &BTreeMap::new()),
    );
    assert_eq!(unpinned.into_iter().collect::<Vec<_>>(), vec![502]);
}

/// A pass that cannot read the tip neither ends a `tip_red` episode nor
/// lets it count as clean evidence: it stays open, marked partial, and a
/// partial episode never enters the gate.
#[test]
fn an_unread_tip_keeps_the_tip_red_episode_open_and_out_of_the_gate() {
    use crate::app::merge_steward_cmd::shadow_ledger;
    let dir = tempfile::tempdir().expect("tempdir");
    let prs = [pr_408(), pr_409()];
    let no_call = |argv: &[String]| -> Result<String, String> {
        Err(format!("no call expected: {}", argv.join(" ")))
    };
    let record = |poison: &RepoPoison| {
        shadow_ledger::record_pass(
            dir.path(),
            "Generous-Corp/vellum",
            "main",
            Some(poison),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &no_call,
            Utc::now(),
        )
    };
    let red = poison(PoisonHold::DryRun, &prs, streak(), tip_red());
    let first = record(&red);
    let open_rules = |report: &serde_json::Value| {
        report["open"]
            .as_array()
            .expect("open")
            .iter()
            .map(|episode| episode["rule"].as_str().unwrap_or("").to_owned())
            .collect::<Vec<_>>()
    };
    assert!(
        open_rules(&first).contains(&"tip_red".to_owned()),
        "{first}"
    );
    let unread = poison(
        PoisonHold::DryRun,
        &prs,
        streak(),
        TipReading {
            error: Some("HTTP 502".to_owned()),
            ..TipReading::default()
        },
    );
    let second = record(&unread);
    assert!(
        second["ended_this_pass"]
            .as_array()
            .expect("ended")
            .iter()
            .all(|episode| episode["rule"] != "tip_red"),
        "{second}"
    );
    let tip_red = second["open"]
        .as_array()
        .expect("open")
        .iter()
        .find(|episode| episode["rule"] == "tip_red")
        .expect("still open");
    assert_eq!(tip_red["partial"], true);
    // Clear: it ends now, partial, and the gate does not count it.
    let clear = RepoPoison {
        health: MergeGroupHealth::Clear {
            detail: "passed".to_owned(),
            passed: vec![CONTEXT.to_owned()],
            passed_by: BTreeMap::from([(CONTEXT.to_owned(), vec![409])]),
        },
        ..poison(PoisonHold::DryRun, &prs, streak(), tip_green())
    };
    let third = record(&clear);
    let gate = third["gate"]
        .as_array()
        .expect("gate")
        .iter()
        .find(|gate| gate["rule"] == "tip_red")
        .expect("tip_red gate")
        .clone();
    assert_eq!(gate["partial"], 1, "{gate}");
    assert_eq!(gate["fires"], 0, "a partial episode is never evidence");
    assert_eq!(third["fix_first"]["pick_repaired"], 0);
}

/// With nothing confirmed, every pull request is admitted as usual: a named
/// fix gets no fast-track and no jump, and no other head is held.
#[test]
fn with_nothing_confirmed_every_pull_request_proceeds() {
    let prs = [pr_408(), pr_409()];
    for tip in [
        tip_green(),
        TipReading {
            error: Some("HTTP 502".to_owned()),
            ..TipReading::default()
        },
    ] {
        let mut unconfirmed = poison(PoisonHold::On, &prs, streak(), tip);
        unconfirmed
            .fixes
            .insert(409, vec!["label `shipyard:unblock`".to_owned()]);
        assert!(unconfirmed.confirmed_contexts().is_empty());
        for pr in [408, 409] {
            assert_eq!(
                base_poison::admission(Some(&unconfirmed), pr),
                Admission::Proceed,
                "#{pr}"
            );
        }
    }
}

/// Live admission for the case the replay test mirrors: a labelled fix is
/// jumped and a second approved head that also passed after the break is
/// admitted, not held.
#[test]
fn live_admission_admits_a_rival_green_head_beside_the_labelled_fix() {
    let mut labelled = pr_409();
    labelled.fact.labels = vec![crate::base_health::fix_first::FIX_LABEL.to_owned()];
    let rival = open_pr(
        410,
        'c',
        None,
        Some("2026-11-13T10:36:00Z"),
        Some("2026-11-13T10:42:00Z"),
    );
    let prs = [pr_408(), labelled, rival];
    let on = poison(PoisonHold::On, &prs, streak(), tip_red());
    assert_eq!(on.fixes.keys().copied().collect::<Vec<_>>(), vec![409]);
    assert_eq!(
        base_poison::admission(Some(&on), 409),
        Admission::FastTrack { jump: true }
    );
    assert_eq!(base_poison::admission(Some(&on), 410), Admission::Proceed);
    assert!(matches!(
        base_poison::admission(Some(&on), 408),
        Admission::Hold { .. }
    ));
}

/// The reorder follows admission's rules: a named fix that does not pass
/// every confirmed context, or any fix on a partial pass, is never moved.
#[test]
fn the_reorder_needs_a_covering_fix_and_a_fully_read_pass() {
    use crate::app::merge_steward_cmd::fix_reorder;
    let mut half = pr_with_greens(500, 'c', &[(CONTEXT, "2026-11-13T10:40:00Z")]);
    half.fact.queue_position = Some(2);
    let mut front = pr_with_greens(
        501,
        'd',
        &[
            (CONTEXT, "2026-11-13T10:40:00Z"),
            (OTHER, "2026-11-13T10:41:00Z"),
        ],
    );
    front.fact.queue_position = Some(1);
    let prs = [front, half];
    let mut poison = with_acts(two_context_poison(&prs, red_on_both()), PoisonHold::On);
    poison.fixes = BTreeMap::from([(500, vec!["label `shipyard:unblock`".to_owned()])]);
    let mut observation = observation_for(prs[0].clone(), true);
    observation.prs = prs.to_vec();
    let refused = fix_reorder::candidate(&poison, &observation).expect_err("not covering");
    assert!(
        refused.contains("does not pass every confirmed context"),
        "{refused}"
    );
    // A covering fix on a partial pass is not moved either.
    let mut whole = pr_with_greens(
        502,
        'e',
        &[
            (CONTEXT, "2026-11-13T10:40:00Z"),
            (OTHER, "2026-11-13T10:41:00Z"),
        ],
    );
    whole.fact.queue_position = Some(3);
    let prs = [prs[0].clone(), whole];
    let mut tip = red_on_both();
    tip.insert(
        "third".to_owned(),
        TipReading {
            error: Some("HTTP 502".to_owned()),
            ..TipReading::default()
        },
    );
    let mut partial = with_acts(two_context_poison(&prs, tip), PoisonHold::On);
    partial.fixes = BTreeMap::from([(502, vec!["green head".to_owned()])]);
    observation.prs = prs.to_vec();
    assert!(partial.covering.contains(&502));
    let refused = fix_reorder::candidate(&partial, &observation).expect_err("partial");
    assert!(refused.contains("partial read"), "{refused}");
}

/// Red-base ejections are not released while another context is still
/// confirmed, or on a partial pass.
#[test]
fn red_base_ejections_wait_for_every_context_and_a_full_read() {
    use crate::app::merge_steward_cmd::red_base_ejections;
    let prs = [ejected_404(), pr_409()];
    let mut observation = observation_for(prs[0].clone(), true);
    observation.prs = prs.to_vec();
    let mut ledger = StewardLedger::default();
    let both = with_acts(two_context_poison(&prs, red_on_both()), PoisonHold::On);
    red_base_ejections::reconcile(
        PoisonHold::On,
        true,
        &mut ledger,
        &observation,
        Some(&both),
        Utc::now(),
    );
    assert_eq!(
        red_base_ejections::entries(&ledger, &observation.repo, &observation.base).len(),
        1
    );
    // CONTEXT goes green on the tip; OTHER is still red.
    let mut tip = red_on_both();
    tip.insert(CONTEXT.to_owned(), tip_green());
    let one_left = two_context_poison(&prs, tip);
    red_base_ejections::reconcile(
        PoisonHold::On,
        true,
        &mut ledger,
        &observation,
        Some(&one_left),
        Utc::now(),
    );
    assert!(!red_base_ejections::entries(&ledger, &observation.repo, &observation.base)[0].ready);
    // Both green but a third read failed: still not released.
    let mut tip = BTreeMap::from([
        (CONTEXT.to_owned(), tip_green()),
        (OTHER.to_owned(), tip_green()),
    ]);
    tip.insert(
        "third".to_owned(),
        TipReading {
            error: Some("HTTP 502".to_owned()),
            ..TipReading::default()
        },
    );
    let partial = two_context_poison(&prs, tip);
    red_base_ejections::reconcile(
        PoisonHold::On,
        true,
        &mut ledger,
        &observation,
        Some(&partial),
        Utc::now(),
    );
    assert!(!red_base_ejections::entries(&ledger, &observation.repo, &observation.base)[0].ready);
    // Fully read, nothing confirmed: released.
    let green = two_context_poison(
        &prs,
        BTreeMap::from([
            (CONTEXT.to_owned(), tip_green()),
            (OTHER.to_owned(), tip_green()),
        ]),
    );
    red_base_ejections::reconcile(
        PoisonHold::On,
        true,
        &mut ledger,
        &observation,
        Some(&green),
        Utc::now(),
    );
    assert!(red_base_ejections::entries(&ledger, &observation.repo, &observation.base)[0].ready);
}

/// A red tip with no red time of its own: nothing is recorded for
/// re-enqueue (no window can be shown) and the reorder never formats the
/// unknown onset. Neither panics.
#[test]
fn an_unknown_onset_records_no_ejection_window_and_never_panics() {
    use crate::app::merge_steward_cmd::{fix_reorder, red_base_ejections};
    let prs = [ejected_404(), pr_409()];
    let mut observation = observation_for(prs[0].clone(), true);
    observation.prs = prs.to_vec();
    let unknown = TipReading {
        red: true,
        ..TipReading::default()
    };
    let red = with_acts(
        poison(PoisonHold::On, &prs, streak(), unknown),
        PoisonHold::On,
    );
    assert_eq!(red.confirmed_contexts(), vec![CONTEXT.to_owned()]);
    let mut ledger = StewardLedger::default();
    let lines = red_base_ejections::reconcile(
        PoisonHold::On,
        true,
        &mut ledger,
        &observation,
        Some(&red),
        Utc::now(),
    );
    assert!(lines.is_empty(), "{lines:?}");
    assert!(red_base_ejections::entries(&ledger, &observation.repo, &observation.base).is_empty());
    let mut queued = red.clone();
    queued.fixes = BTreeMap::from([(409, vec!["label `shipyard:unblock`".to_owned()])]);
    let mut queued_prs = prs.to_vec();
    queued_prs[1].fact.queue_position = Some(2);
    observation.prs = queued_prs;
    // Refused (nothing covers an unknown onset), without formatting it.
    assert!(fix_reorder::candidate(&queued, &observation).is_err());
}
