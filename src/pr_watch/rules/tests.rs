use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{Value, json};

use super::read::{read_alarms, read_inputs, read_trails, review_entry};
use super::{
    Alarm, InputEvent, RuleConfig, RuleEvent, RuleLedger, TrailEntry, WatchInput, evaluate, pass,
    ready_head, replay,
};
use crate::pr_watch::{
    CheckFact, GroupRun, HeadFact, PrHistory, QueueEvent, QueueEventKind, RepoHistory,
};

const REPO: &str = "o/r";
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OLD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn t(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap() + Duration::minutes(minutes)
}

fn check(name: &str, id: u64, start: i64, end: i64, conclusion: &str, tests: &[&str]) -> CheckFact {
    CheckFact {
        name: name.to_owned(),
        id,
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: Some(t(start)),
        completed_at: Some(t(end)),
        signatures: tests.iter().map(|test| (*test).to_owned()).collect(),
    }
}

fn pr(events: Vec<(i64, QueueEventKind)>, checks: Vec<CheckFact>) -> PrHistory {
    PrHistory {
        number: 7,
        title: "t".to_owned(),
        created_at: Some(t(-120)),
        head_sha: HEAD.to_owned(),
        heads: vec![
            HeadFact {
                sha: OLD.to_owned(),
                first_seen_at: t(-120),
                ..HeadFact::default()
            },
            HeadFact {
                sha: HEAD.to_owned(),
                first_seen_at: t(0),
                checks,
                ..HeadFact::default()
            },
        ],
        events: events
            .into_iter()
            .map(|(at, kind)| QueueEvent { at: t(at), kind })
            .collect(),
        timeline_complete: true,
        author: Some("shipyard-local".to_owned()),
        ..PrHistory::default()
    }
}

fn history(prs: Vec<PrHistory>, groups: Vec<GroupRun>) -> RepoHistory {
    RepoHistory {
        repo: REPO.to_owned(),
        base: "main".to_owned(),
        from: t(-600),
        to: t(600),
        required_checks: vec!["macos".to_owned()],
        prs: prs.into_iter().map(|pr| (pr.number, pr)).collect(),
        group_runs: groups,
        gaps: Vec::new(),
    }
}

fn comment(id: u64, at: i64, body: &str) -> TrailEntry {
    TrailEntry::from_comment(id, "shipyard-local[bot]", t(at), body)
}

/// A comment by `login`, with the `author_association` GitHub reported.
fn by(id: u64, at: i64, login: &str, association: Option<&str>, body: &str) -> TrailEntry {
    let mut entry = TrailEntry::from_comment(id, login, t(at), body);
    entry.association = association.map(str::to_owned);
    entry
}

fn input(pr: PrHistory, trail: Vec<TrailEntry>) -> WatchInput {
    WatchInput {
        repos: vec![history(vec![pr], Vec::new())],
        trails: BTreeMap::from([(REPO.to_owned(), BTreeMap::from([(7, trail)]))]),
        ..WatchInput::default()
    }
}

fn kinds(events: &[RuleEvent]) -> BTreeSet<String> {
    events.iter().map(|event| event.kind.clone()).collect()
}

fn holding(input: &WatchInput, at: i64) -> BTreeSet<String> {
    kinds(&evaluate(input, t(at), &RuleConfig::default(), false))
}

#[test]
fn ready_announcements_are_read_from_the_first_line() {
    assert_eq!(
        ready_head(&format!("READY head:{HEAD}")),
        Some(HEAD.to_owned())
    );
    assert_eq!(
        ready_head(&format!(
            "\n**READY** for review: head `{HEAD}` (read from the API)\nmore"
        )),
        Some(HEAD.to_owned())
    );
    assert_eq!(ready_head(&format!("ready: {HEAD}")), Some(HEAD.to_owned()));
    assert_eq!(ready_head(&format!("Not READY: {HEAD}")), None);
    assert_eq!(ready_head(&format!("Already merged {HEAD}")), None);
    assert_eq!(ready_head(&format!("Review\nREADY head:{HEAD}")), None);
    assert_eq!(ready_head("READY head:aaaaaaa"), None);
    assert_eq!(
        ready_head(&format!("READY head:{HEAD}a")),
        None,
        "41 hex is not a SHA"
    );
}

#[test]
fn a_ready_head_is_routed_until_a_verdict_names_it() {
    let trail = vec![
        comment(1, 10, &format!("READY head:{HEAD}")),
        comment(2, 40, &format!("Review.\n\nverdict: APPROVE head:{HEAD}")),
    ];
    let watch = input(pr(Vec::new(), Vec::new()), trail);
    assert!(!holding(&watch, 5).contains("ready_unreviewed"));
    assert!(holding(&watch, 20).contains("ready_unreviewed"));
    let after = holding(&watch, 50);
    assert!(!after.contains("ready_unreviewed"));
    assert!(after.contains("verdict_posted"));
}

#[test]
fn a_ready_naming_an_older_head_does_not_count_for_the_current_one() {
    let watch = input(
        pr(Vec::new(), Vec::new()),
        vec![comment(1, 10, &format!("READY head:{OLD}"))],
    );
    assert!(!holding(&watch, 20).contains("ready_unreviewed"));
}

#[test]
fn a_head_with_no_ready_and_no_verdict_is_reported_only_when_asked() {
    let watch = input(pr(Vec::new(), Vec::new()), Vec::new());
    assert!(
        !holding(&watch, 600).contains("head_unreviewed"),
        "off by default"
    );
    let asked = RuleConfig {
        head_unreviewed_after: Some(Duration::minutes(45)),
        ..RuleConfig::default()
    };
    let at = |watch: &WatchInput, minutes| kinds(&evaluate(watch, t(minutes), &asked, false));
    assert!(!at(&watch, 44).contains("head_unreviewed"));
    assert!(at(&watch, 45).contains("head_unreviewed"));
    let mut draft = pr(Vec::new(), Vec::new());
    draft.draft = true;
    assert!(!at(&input(draft, Vec::new()), 90).contains("head_unreviewed"));
}

/// pulp#11117: a HOLD, then a later APPROVE of the same unchanged head, then
/// an arm. The HOLD makes the head un-approvable, so the arm is reported.
#[test]
fn a_later_approve_does_not_clear_a_hold_on_the_same_armed_head() {
    let armed = pr(vec![(30, QueueEventKind::Armed)], Vec::new());
    let trail = vec![
        comment(1, 10, &format!("verdict: HOLD head:{HEAD}")),
        comment(2, 20, &format!("verdict: APPROVE head:{HEAD}")),
    ];
    let events = evaluate(
        &input(armed.clone(), trail),
        t(31),
        &RuleConfig::default(),
        false,
    );
    let event = events
        .iter()
        .find(|event| event.kind == "armed_unapproved")
        .expect("a held head armed after a later APPROVE");
    assert!(event.summary.contains("HOLD"), "{}", event.summary);
    // Control: the HOLD names an older head, so the APPROVE stands.
    let moved = vec![
        comment(1, 10, &format!("verdict: HOLD head:{OLD}")),
        comment(2, 20, &format!("verdict: APPROVE head:{HEAD}")),
    ];
    assert!(!holding(&input(armed, moved), 31).contains("armed_unapproved"));
}

#[test]
fn an_armed_head_needs_the_latest_verdict_on_it_to_approve() {
    let armed = pr(vec![(30, QueueEventKind::Armed)], Vec::new());
    assert!(holding(&input(armed.clone(), Vec::new()), 31).contains("armed_unapproved"));
    let approved = vec![comment(1, 20, &format!("verdict: APPROVE head:{HEAD}"))];
    assert!(!holding(&input(armed.clone(), approved.clone()), 31).contains("armed_unapproved"));
    let mut held = approved;
    held.push(comment(2, 25, &format!("verdict: HOLD head:{HEAD}")));
    let events = evaluate(
        &input(armed.clone(), held),
        t(31),
        &RuleConfig::default(),
        false,
    );
    let event = events
        .iter()
        .find(|event| event.kind == "armed_unapproved")
        .expect("held head armed");
    assert!(event.summary.contains("HOLD"), "{}", event.summary);
    // An approval of an older head does not cover this one.
    let stale = vec![comment(1, 20, &format!("reviewed: {OLD}"))];
    assert!(holding(&input(armed, stale), 31).contains("armed_unapproved"));
    // Disarmed again: nothing is in flight.
    let disarmed = pr(
        vec![
            (30, QueueEventKind::Armed),
            (32, QueueEventKind::Disarmed { reason: None }),
        ],
        Vec::new(),
    );
    assert!(!holding(&input(disarmed, Vec::new()), 33).contains("armed_unapproved"));
    // A queue entry counts as armed even without an arm event.
    let queued = pr(vec![(30, QueueEventKind::Enqueued)], Vec::new());
    assert!(holding(&input(queued, Vec::new()), 31).contains("armed_unapproved"));
}

#[test]
fn a_red_required_check_carries_its_failing_tests() {
    let red = pr(
        vec![(1, QueueEventKind::Armed)],
        vec![check(
            "macos",
            9,
            2,
            40,
            "failure",
            &["census-drift", "gpu-locks"],
        )],
    );
    let watch = input(red, Vec::new());
    assert!(!holding(&watch, 39).contains("required_red"));
    let events = evaluate(&watch, t(41), &RuleConfig::default(), false);
    let event = events
        .iter()
        .find(|event| event.kind == "required_red")
        .expect("red");
    assert!(
        event.summary.contains("failing: census-drift, gpu-locks"),
        "{}",
        event.summary
    );
    assert_eq!(event.detail["tests"], json!(["census-drift", "gpu-locks"]));
    assert_eq!(event.detail["armed"], json!(true));
}

#[test]
fn a_cancelled_check_is_red_and_a_green_rerun_clears_it() {
    let checks = vec![
        check("macos", 9, 2, 40, "cancelled", &[]),
        check("macos", 10, 45, 80, "success", &[]),
    ];
    let watch = input(pr(vec![(1, QueueEventKind::Armed)], checks), Vec::new());
    assert!(holding(&watch, 41).contains("required_red"));
    assert!(!holding(&watch, 46).contains("required_red"));
}

#[test]
fn a_red_head_nobody_armed_or_approved_is_left_to_its_author() {
    let checks = vec![check("macos", 9, 2, 40, "failure", &["census-drift"])];
    let unarmed = input(pr(Vec::new(), checks.clone()), Vec::new());
    assert!(!holding(&unarmed, 41).contains("required_red"));
    let approved = input(
        pr(Vec::new(), checks),
        vec![comment(1, 30, &format!("verdict: APPROVE head:{HEAD}"))],
    );
    assert!(holding(&approved, 41).contains("required_red"));
}

#[test]
fn a_failed_merge_group_names_its_tests() {
    let group = GroupRun {
        id: 77,
        pr: Some(7),
        head_sha: "c".repeat(40),
        created_at: t(10),
        conclusion: Some("failure".to_owned()),
        required_jobs: vec![check("macos", 5, 10, 50, "failure", &["drift-fast"])],
        ..GroupRun::default()
    };
    let watch = WatchInput {
        repos: vec![history(vec![pr(Vec::new(), Vec::new())], vec![group])],
        ..WatchInput::default()
    };
    let events = evaluate(&watch, t(51), &RuleConfig::default(), false);
    let event = events
        .iter()
        .find(|event| event.kind == "group_red")
        .expect("group red");
    assert!(
        event.summary.contains("failing: drift-fast"),
        "{}",
        event.summary
    );
    assert!(!holding(&watch, 49).contains("group_red"));
}

#[test]
fn queue_removals_and_long_waits_are_reported_but_merges_are_not() {
    let removed = pr(
        vec![
            (10, QueueEventKind::Enqueued),
            (
                20,
                QueueEventKind::Removed {
                    reason: "failed_checks".to_owned(),
                },
            ),
        ],
        Vec::new(),
    );
    assert!(holding(&input(removed, Vec::new()), 21).contains("queue_removed"));
    let stuck = pr(vec![(10, QueueEventKind::Enqueued)], Vec::new());
    assert!(!holding(&input(stuck.clone(), Vec::new()), 99).contains("queue_stuck"));
    assert!(holding(&input(stuck, Vec::new()), 100).contains("queue_stuck"));
    let mut merged = pr(
        vec![
            (10, QueueEventKind::Enqueued),
            (
                20,
                QueueEventKind::Removed {
                    reason: "merged".to_owned(),
                },
            ),
        ],
        Vec::new(),
    );
    merged.merged_at = Some(t(20));
    let after = holding(&input(merged, Vec::new()), 30);
    assert!(!after.contains("queue_removed"));
    assert!(!after.contains("queue_stuck"));
}

#[test]
fn an_approved_head_nobody_armed_is_reported_after_the_wait() {
    let approved = vec![comment(1, 20, &format!("verdict: APPROVE head:{HEAD}"))];
    let watch = input(pr(Vec::new(), Vec::new()), approved.clone());
    assert!(!holding(&watch, 34).contains("approved_unarmed"));
    assert!(holding(&watch, 35).contains("approved_unarmed"));
    let armed = input(pr(vec![(30, QueueEventKind::Armed)], Vec::new()), approved);
    assert!(!holding(&armed, 40).contains("approved_unarmed"));
}

#[test]
fn an_approved_dirty_head_is_reported_only_by_a_live_pass() {
    let mut dirty = pr(Vec::new(), Vec::new());
    dirty.merge_state = Some("DIRTY".to_owned());
    let watch = input(
        dirty,
        vec![comment(1, 20, &format!("verdict: APPROVE head:{HEAD}"))],
    );
    assert!(!holding(&watch, 40).contains("approved_blocked"));
    assert!(
        kinds(&evaluate(&watch, t(40), &RuleConfig::default(), true)).contains("approved_blocked")
    );
}

#[test]
fn alarms_and_input_events_pass_through_once_open() {
    let watch = WatchInput {
        alarms: vec![Alarm {
            repo: "o/tartci".to_owned(),
            number: 504,
            title: "[tartci] m5: cannot boot VMs".to_owned(),
            opened_at: t(10),
            closed_at: None,
        }],
        inputs: vec![InputEvent {
            kind: "codex_session_errored".to_owned(),
            key: "ws14@1".to_owned(),
            at: t(15),
            rest: BTreeMap::from([("summary".to_owned(), json!("ws14 errored"))]),
        }],
        ..WatchInput::default()
    };
    assert!(holding(&watch, 9).is_empty());
    let events = evaluate(&watch, t(16), &RuleConfig::default(), false);
    assert_eq!(
        kinds(&events),
        BTreeSet::from(["host_alarm".to_owned(), "codex_session_errored".to_owned()])
    );
    assert!(events.iter().any(|event| event.summary == "ws14 errored"));
}

#[test]
fn the_ledger_emits_each_key_once_and_a_first_pass_only_primes() {
    let watch = input(
        pr(vec![(30, QueueEventKind::Armed)], Vec::new()),
        Vec::new(),
    );
    let (ledger, first) = pass(&watch, None, t(31), &RuleConfig::default());
    assert!(first.primed);
    assert!(first.events.is_empty());
    assert!(
        ledger
            .seen
            .keys()
            .any(|key| key.starts_with("armed_unapproved:"))
    );
    let (ledger, second) = pass(&watch, Some(ledger), t(32), &RuleConfig::default());
    assert!(second.events.is_empty(), "{:?}", second.events);
    // A new head is a new key.
    let mut moved = pr(vec![(30, QueueEventKind::Armed)], Vec::new());
    moved.heads.push(HeadFact {
        sha: "d".repeat(40),
        first_seen_at: t(33),
        ..HeadFact::default()
    });
    let moved = input(moved, Vec::new());
    let (ledger, third) = pass(&moved, Some(ledger), t(34), &RuleConfig::default());
    assert_eq!(
        kinds(&third.events),
        BTreeSet::from(["armed_unapproved".to_owned()])
    );
    // ...and is recorded, so the next pass is quiet again.
    let (_, fourth) = pass(&moved, Some(ledger), t(35), &RuleConfig::default());
    assert!(fourth.events.is_empty(), "{:?}", fourth.events);
}

#[test]
fn a_ledger_drops_keys_after_two_weeks() {
    let mut ledger = RuleLedger::new();
    ledger
        .seen
        .insert("old".to_owned(), t(0) - Duration::days(15));
    ledger.seen.insert("new".to_owned(), t(0));
    // Just inside the fortnight is kept; just outside is dropped.
    ledger.seen.insert(
        "thirteen-days".to_owned(),
        t(0) - Duration::days(14) + Duration::seconds(1),
    );
    ledger.seen.insert(
        "fourteen-days".to_owned(),
        t(0) - Duration::days(14) - Duration::seconds(1),
    );
    ledger.prune(t(0));
    assert_eq!(
        ledger.seen.keys().collect::<Vec<_>>(),
        vec!["new", "thirteen-days"]
    );
}

#[test]
fn every_event_carries_a_complete_fix() {
    let red = pr(
        vec![
            (1, QueueEventKind::Enqueued),
            (
                2,
                QueueEventKind::Removed {
                    reason: "failed_checks".to_owned(),
                },
            ),
            (3, QueueEventKind::Armed),
            (4, QueueEventKind::Enqueued),
        ],
        vec![check("macos", 9, 2, 40, "failure", &["census-drift"])],
    );
    let trail = vec![
        comment(1, 10, &format!("READY head:{HEAD}")),
        comment(2, -10, &format!("verdict: HOLD head:{OLD}")),
    ];
    let group = GroupRun {
        id: 77,
        pr: Some(7),
        head_sha: "c".repeat(40),
        created_at: t(10),
        conclusion: Some("failure".to_owned()),
        required_jobs: vec![check("macos", 5, 10, 50, "failure", &["drift-fast"])],
        ..GroupRun::default()
    };
    let mut watch = input(red, trail);
    watch.repos[0].group_runs.push(group);
    watch.alarms.push(Alarm {
        repo: "o/tartci".to_owned(),
        number: 504,
        title: "[tartci] m5: cannot boot VMs".to_owned(),
        opened_at: t(10),
        closed_at: None,
    });
    let config = RuleConfig {
        head_unreviewed_after: Some(Duration::minutes(45)),
        ..RuleConfig::default()
    };
    let events = evaluate(&watch, t(200), &config, true);
    let seen = kinds(&events);
    for kind in [
        "ready_unreviewed",
        "verdict_posted",
        "armed_unapproved",
        "required_red",
        "group_red",
        "queue_removed",
        "queue_stuck",
        "host_alarm",
    ] {
        assert!(seen.contains(kind), "{kind} not raised: {seen:?}");
    }
    for event in &events {
        let fix = &event.fix;
        for (field, value) in [
            ("what", &fix.what),
            ("evidence", &fix.evidence),
            ("cause", &fix.cause),
            ("command", &fix.command),
            ("verify", &fix.verify),
        ] {
            assert!(!value.is_empty(), "{} has no {field}", event.kind);
        }
    }
    let red = events
        .iter()
        .find(|event| event.kind == "required_red")
        .expect("red");
    assert_eq!(red.fix.cause, "test_failure");
    assert!(red.fix.what.contains("census-drift"), "{}", red.fix.what);
    assert_eq!(red.fix.evidence, "https://github.com/o/r/runs/9");
    assert!(
        red.fix.command.contains("--job 9 --log-failed"),
        "{}",
        red.fix.command
    );
    let group = events
        .iter()
        .find(|event| event.kind == "group_red")
        .expect("group");
    assert_eq!(group.fix.evidence, "https://github.com/o/r/actions/runs/77");
}

#[test]
fn a_ready_head_is_routed_to_the_configured_reviewer() {
    let watch = input(
        pr(Vec::new(), Vec::new()),
        vec![comment(5, 10, &format!("READY head:{HEAD}"))],
    );
    let unrouted = evaluate(&watch, t(20), &RuleConfig::default(), false);
    let event = unrouted
        .iter()
        .find(|event| event.kind == "ready_unreviewed")
        .expect("ready");
    assert!(!event.fix.safe_to_auto_apply, "no reviewer to route to");
    assert_eq!(
        event.fix.evidence,
        "https://github.com/o/r/pull/7#issuecomment-5"
    );
    let config = RuleConfig {
        reviewers: BTreeMap::from([("o/r".to_owned(), "fable-graph".to_owned())]),
        ..RuleConfig::default()
    };
    let routed = evaluate(&watch, t(20), &config, false);
    let event = routed
        .iter()
        .find(|event| event.kind == "ready_unreviewed")
        .expect("ready");
    assert!(
        event.fix.command.contains("to fable-graph"),
        "{}",
        event.fix.command
    );
    assert!(
        event
            .fix
            .command
            .contains(&format!("`verdict: APPROVE head:{HEAD}`"))
    );
    assert!(event.fix.safe_to_auto_apply);
}

/// The repositories are public: only the fleet App or an account the
/// repository trusts approves a head, and never the pull request's author.
/// Anything else neither yields an auto-safe arm (on a queue-less repository
/// that arm is the merge) nor reaches the author as a reviewer's verdict.
#[test]
fn only_a_trusted_verdict_approves_a_head_or_reaches_the_author() {
    let approve = format!("verdict: APPROVE head:{HEAD}");
    let mut by_alice = pr(Vec::new(), Vec::new());
    by_alice.author = Some("alice".to_owned());
    let fired = |trail: Vec<TrailEntry>| {
        kinds(&evaluate(
            &input(by_alice.clone(), trail),
            t(40),
            &RuleConfig::default(),
            false,
        ))
    };
    for untrusted in [
        by(1, 20, "drive-by", Some("NONE"), &approve),
        by(1, 20, "contributor", Some("CONTRIBUTOR"), &approve),
        by(1, 20, "unknown", None, &approve),
        by(1, 20, "alice", Some("OWNER"), &approve),
    ] {
        let events = fired(vec![untrusted.clone()]);
        assert!(
            !events.contains("approved_unarmed"),
            "{untrusted:?}: {events:?}"
        );
        assert!(
            !events.contains("verdict_posted"),
            "{untrusted:?}: {events:?}"
        );
    }
    // Control: the same comment from an owner who is not the author, or from
    // the fleet App, approves the head and is relayed.
    for trusted in [
        by(1, 20, "daniel", Some("OWNER"), &approve),
        by(1, 20, "member", Some("MEMBER"), &approve),
        by(1, 20, "shipyard-local[bot]", None, &approve),
    ] {
        let events = fired(vec![trusted.clone()]);
        assert!(
            events.contains("approved_unarmed"),
            "{trusted:?}: {events:?}"
        );
        assert!(events.contains("verdict_posted"), "{trusted:?}: {events:?}");
    }
    // An outsider's HOLD does not block a trusted approval either.
    let events = fired(vec![
        by(1, 20, "daniel", Some("OWNER"), &approve),
        by(
            2,
            25,
            "drive-by",
            Some("NONE"),
            &format!("verdict: HOLD head:{HEAD}"),
        ),
    ]);
    assert!(events.contains("approved_unarmed"), "{events:?}");
}

#[test]
fn an_approved_head_nobody_armed_gets_the_bound_arm_command() {
    let watch = input(
        pr(Vec::new(), Vec::new()),
        vec![comment(1, 20, &format!("verdict: APPROVE head:{HEAD}"))],
    );
    let events = evaluate(&watch, t(40), &RuleConfig::default(), false);
    let event = events
        .iter()
        .find(|event| event.kind == "approved_unarmed")
        .expect("approved");
    assert_eq!(
        event.fix.command,
        format!("ghapp pr merge 7 --repo o/r --auto --merge --match-head-commit {HEAD}")
    );
    assert!(event.fix.safe_to_auto_apply);
}

#[test]
fn an_input_events_fix_passes_through_and_a_missing_one_says_so() {
    let fix = json!({"what": "w", "evidence": "e", "cause": "c", "command": "x",
                     "safe_to_auto_apply": true, "verify": "v"});
    let watch = WatchInput {
        inputs: vec![
            InputEvent {
                kind: "codex_session_errored".to_owned(),
                key: "a".to_owned(),
                at: t(1),
                rest: BTreeMap::from([("fix".to_owned(), fix)]),
            },
            InputEvent {
                kind: "codex_session_idle".to_owned(),
                key: "b".to_owned(),
                at: t(1),
                rest: BTreeMap::new(),
            },
        ],
        ..WatchInput::default()
    };
    let events = evaluate(&watch, t(2), &RuleConfig::default(), false);
    assert_eq!(events[0].fix.command, "x");
    assert!(events[0].fix.safe_to_auto_apply);
    assert_eq!(events[1].fix.cause, "input_without_fix");
    assert!(!events[1].fix.safe_to_auto_apply);
}

#[test]
fn replay_stamps_each_event_with_the_first_tick_and_skips_what_held_at_the_start() {
    let trail = vec![
        comment(1, -30, &format!("verdict: HOLD head:{OLD}")),
        comment(2, 12, &format!("READY head:{HEAD}")),
    ];
    let watch = input(pr(vec![(-50, QueueEventKind::Armed)], Vec::new()), trail);
    let events = replay(
        &watch,
        t(0),
        t(60),
        Duration::minutes(5),
        &RuleConfig::default(),
    );
    // The HOLD on the old head was posted before the window: primed, not emitted.
    assert!(!events.iter().any(|event| event.kind == "verdict_posted"));
    let ready = events
        .iter()
        .find(|event| event.kind == "ready_unreviewed")
        .expect("ready");
    assert_eq!(ready.at, t(15));
    assert_eq!(ready.since, Some(t(12)));
    // Armed before the window on the old head; the new head is a new key at t(0)
    // and so already held when the window began.
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "armed_unapproved")
            .count(),
        0
    );
    let keys: BTreeSet<&str> = events.iter().map(|event| event.key.as_str()).collect();
    assert_eq!(keys.len(), events.len(), "a key fires once");
}

fn fake_gh(
    answers: Vec<(&'static str, String)>,
) -> impl Fn(&[String]) -> Result<String, String> + Sync {
    move |argv: &[String]| {
        let joined = argv.join(" ");
        answers
            .iter()
            .find(|(needle, _)| joined.contains(needle))
            .map(|(_, answer)| answer.clone())
            .ok_or_else(|| format!("unscripted gh call: {joined}"))
    }
}

#[test]
fn trails_come_from_one_repository_listing_and_batched_reviews() {
    let comments = json!([[
        {"id": 1, "user": {"login": "shipyard-local[bot]"}, "created_at": "2026-10-11T12:10:00Z",
         "author_association": "NONE",
         "issue_url": "https://api.github.com/repos/o/r/issues/7", "body": format!("READY head:{HEAD}")},
        {"id": 2, "user": {"login": "codecov[bot]"}, "created_at": "2026-10-11T12:11:00Z",
         "issue_url": "https://api.github.com/repos/o/r/issues/7", "body": "## Codecov report"},
        {"id": 3, "user": {"login": "reviewer"}, "created_at": "2026-10-11T12:12:00Z",
         "issue_url": "https://api.github.com/repos/o/r/issues/8", "body": format!("reviewed: {HEAD}")}
    ]]);
    let reviews = json!({"data": {"repository": {"p7": {"reviews": {"nodes": [
        {"databaseId": 50, "author": {"login": "reviewer"}, "authorAssociation": "MEMBER",
         "state": "CHANGES_REQUESTED", "submittedAt": "2026-10-11T12:20:00Z", "commit": {"oid": HEAD}},
        {"databaseId": 51, "author": {"login": "reviewer"}, "state": "COMMENTED",
         "submittedAt": "2026-10-11T12:21:00Z", "commit": {"oid": HEAD}}
    ]}}}}});
    // The review query must ask for the association, or reviews never count.
    let gh = fake_gh(vec![
        ("repos/o/r/issues/comments", comments.to_string()),
        ("authorAssociation", reviews.to_string()),
    ]);
    let trails = read_trails(&gh, REPO, &BTreeSet::from([7]), t(-60)).expect("read");
    let trail = &trails[&7];
    assert_eq!(trail.len(), 2, "{trail:?}");
    assert_eq!(trail[0].ready_head.as_deref(), Some(HEAD));
    assert_eq!(trail[0].association.as_deref(), Some("NONE"));
    assert!(trail[1].review);
    assert_eq!(trail[1].association.as_deref(), Some("MEMBER"));
    assert_eq!(
        trail[1].verdict.as_ref().map(|verdict| verdict.approve),
        Some(false)
    );
    assert!(!trails.contains_key(&8), "pull request 8 is not watched");
}

#[test]
fn a_pull_requests_whole_trail_is_read_from_its_own_comments() {
    let comments = json!([[
        {"id": 1, "user": {"login": "shipyard-local[bot]"}, "created_at": "2026-09-01T12:00:00Z",
         "body": format!("verdict: APPROVE head:{HEAD}")},
        {"id": 2, "user": {"login": "codecov[bot]"}, "created_at": "2026-09-01T12:01:00Z",
         "body": "## Codecov report"}
    ]]);
    let reviews = json!({"data": {"repository": {"p7": {"reviews": {"nodes": []}}}}});
    let gh = fake_gh(vec![
        ("repos/o/r/issues/7/comments", comments.to_string()),
        ("graphql", reviews.to_string()),
    ]);
    let trail = super::read::read_pr_trail(&gh, REPO, 7).expect("read");
    assert_eq!(trail.len(), 1);
    assert_eq!(
        trail[0]
            .verdict
            .as_ref()
            .map(|verdict| verdict.head.as_str()),
        Some(HEAD)
    );
}

#[test]
fn the_rate_budget_is_the_smaller_of_core_and_graphql() {
    let answer = json!({"resources": {
        "core": {"remaining": 4200, "reset": 1_791_860_000},
        "graphql": {"remaining": 900, "reset": 1_791_861_000}}});
    let gh = fake_gh(vec![("rate_limit", answer.to_string())]);
    let budget = super::read::read_rate_budget(&gh).expect("read");
    assert_eq!(budget.remaining(), 900);
    assert_eq!(
        budget.reset.map(|at| at.timestamp()),
        Some(1_791_861_000),
        "the exhausted budget's reset"
    );
    assert!(super::read::rate_limited(
        "gh: API rate limit exceeded for installation ID 123. (HTTP 403)"
    ));
    assert!(super::read::rate_limited(
        "You have exceeded a secondary rate limit"
    ));
    assert!(!super::read::rate_limited("HTTP 502: Bad Gateway"));
}

#[test]
fn an_unreadable_trail_is_an_error_not_an_empty_one() {
    let gh = fake_gh(Vec::new());
    assert!(read_trails(&gh, REPO, &BTreeSet::from([7]), t(0)).is_err());
    let errors = fake_gh(vec![
        ("repos/o/r/issues/comments", "[[]]".to_owned()),
        (
            "graphql",
            json!({"errors": [{"message": "boom"}]}).to_string(),
        ),
    ]);
    assert!(read_trails(&errors, REPO, &BTreeSet::from([7]), t(0)).is_err());
}

#[test]
fn only_tartci_issues_are_alarms() {
    let issues = json!([[
        {"number": 504, "title": "[tartci] m5: cannot boot VMs since 2026-10-12T17:56:37Z",
         "created_at": "2026-10-12T18:11:17Z", "closed_at": "2026-10-12T19:38:18Z"},
        {"number": 484, "title": "Interpreter pin follow-ups", "created_at": "2026-10-11T23:04:42Z"},
        {"number": 500, "title": "[tartci] a pull request", "created_at": "2026-10-11T23:04:42Z",
         "pull_request": {}}
    ]]);
    let gh = fake_gh(vec![("repos/o/tartci/issues", issues.to_string())]);
    let alarms = read_alarms(&gh, "o/tartci", t(0)).expect("read");
    assert_eq!(alarms.len(), 1);
    assert_eq!(alarms[0].number, 504);
    assert!(alarms[0].closed_at.is_some());
}

#[test]
fn a_review_that_is_not_a_verdict_is_ignored() {
    let node = |state: &str| {
        json!({"databaseId": 1, "author": {"login": "r"}, "state": state,
               "submittedAt": "2026-10-11T12:00:00Z", "commit": {"oid": HEAD}})
    };
    assert!(
        review_entry(&node("APPROVED"))
            .is_some_and(|entry| entry.verdict.is_some_and(|v| v.approve))
    );
    assert!(review_entry(&node("COMMENTED")).is_none());
    assert!(review_entry(&node("DISMISSED")).is_none());
}

#[test]
fn input_event_files_reject_a_malformed_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let good = dir.path().join("good.jsonl");
    std::fs::write(
        &good,
        "{\"kind\":\"codex_session_idle\",\"key\":\"ws1@1\",\"at\":\"2026-10-11T12:00:00Z\",\"summary\":\"ws1 idle\"}\n\n",
    )
    .expect("write");
    let events = read_inputs(&good).expect("read");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].rest.get("summary"),
        Some(&Value::from("ws1 idle"))
    );
    let bad = dir.path().join("bad.jsonl");
    std::fs::write(&bad, "{\"kind\":\"x\"}\n").expect("write");
    assert!(read_inputs(&bad).is_err());
}

#[test]
fn a_second_recorder_in_the_same_directory_keeps_the_first_ones_answers() {
    use crate::pr_watch::fixtures::{FixtureReader, Recorder};
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Recorder::new(dir.path()).expect("recorder");
    first.record(
        &["api".to_owned(), "repos/o/r/branches/main".to_owned()],
        "{}",
    );
    drop(first);
    let second = Recorder::new(dir.path()).expect("recorder");
    second.record(&["api".to_owned(), "repos/o/r/issues".to_owned()], "[]");
    let fixtures = FixtureReader::load(dir.path()).expect("load");
    assert_eq!(
        fixtures.read(&["api".to_owned(), "repos/o/r/branches/main".to_owned()]),
        Ok("{}".to_owned())
    );
    assert_eq!(
        fixtures.read(&["api".to_owned(), "repos/o/r/issues".to_owned()]),
        Ok("[]".to_owned())
    );
}

fn hold_trail(at: i64) -> Vec<TrailEntry> {
    vec![comment(
        1,
        at,
        &format!("Findings.\n\nverdict: HOLD head:{HEAD}"),
    )]
}

#[test]
fn a_hold_nobody_repaired_is_reported_after_held_after_with_its_age() {
    let watch = input(pr(Vec::new(), Vec::new()), hold_trail(10));
    assert!(
        !holding(&watch, 60).contains("held_unrepaired"),
        "inside 60m"
    );
    let events = evaluate(&watch, t(100), &RuleConfig::default(), false);
    let event = events
        .iter()
        .find(|event| event.kind == "held_unrepaired")
        .expect("held");
    assert_eq!(event.since, Some(t(10)));
    assert_eq!(event.age_secs, Some(90 * 60));
    assert_eq!(event.age.as_deref(), Some("1h30m"));
    assert!(!event.fix.safe_to_auto_apply);
    let config = RuleConfig {
        held_after: Duration::minutes(10),
        ..RuleConfig::default()
    };
    assert!(kinds(&evaluate(&watch, t(30), &config, false)).contains("held_unrepaired"));
}

#[test]
fn a_hold_is_unrepaired_until_a_new_head_follows() {
    // Control: the same HOLD on the current head reports.
    assert!(
        holding(&input(pr(Vec::new(), Vec::new()), hold_trail(10)), 100)
            .contains("held_unrepaired")
    );
    // A newer approval of the same head does not supersede the HOLD: a HOLD
    // makes its head un-approvable, so only a push repairs it.
    let mut trail = hold_trail(10);
    trail.push(comment(2, 30, &format!("verdict: APPROVE head:{HEAD}")));
    assert!(holding(&input(pr(Vec::new(), Vec::new()), trail), 100).contains("held_unrepaired"));
    // A push after the HOLD: the verdict names a head that is no longer current.
    let mut pushed = pr(Vec::new(), Vec::new());
    pushed.head_sha = OLD.to_owned();
    pushed.heads[0].sha = HEAD.to_owned();
    pushed.heads[0].first_seen_at = t(-120);
    pushed.heads[1].sha = OLD.to_owned();
    pushed.heads[1].first_seen_at = t(50);
    assert!(!holding(&input(pushed, hold_trail(10)), 100).contains("held_unrepaired"));
    // Armed heads are armed_unapproved's, not reported twice.
    let armed = pr(vec![(20, QueueEventKind::Armed)], Vec::new());
    let both = holding(&input(armed, hold_trail(10)), 100);
    assert!(both.contains("armed_unapproved"));
    assert!(!both.contains("held_unrepaired"));
}

#[test]
fn labelled_pull_requests_get_their_own_state_and_no_stall_rule() {
    for (label, state) in [
        ("owner-decision", "owner_decision"),
        ("Superseded", "superseded"),
        ("PARKED/waiting-on-vendor", "parked"),
    ] {
        let mut labelled = pr(vec![(20, QueueEventKind::Armed)], Vec::new());
        labelled.labels = vec!["bug".to_owned(), label.to_owned()];
        let watch = input(labelled, hold_trail(10));
        let events = evaluate(&watch, t(100), &RuleConfig::default(), false);
        let seen = kinds(&events);
        assert!(seen.contains(state), "{label}: {seen:?}");
        for stall in ["held_unrepaired", "armed_unapproved", "queue_stuck"] {
            assert!(!seen.contains(stall), "{label}: {stall}");
        }
        let event = events.iter().find(|event| event.kind == state).unwrap();
        assert_eq!(event.age_secs, Some(100 * 60), "age is the head's");
        assert_eq!(event.detail["age_basis"], "head_first_seen");
    }
    // Control: near-miss labels do not set the pull request aside.
    let mut plain = pr(Vec::new(), Vec::new());
    plain.labels = vec!["parked".to_owned(), "owner-decisions".to_owned()];
    let seen = holding(&input(plain, hold_trail(10)), 100);
    assert!(seen.contains("held_unrepaired"));
    assert!(!seen.contains("parked") && !seen.contains("owner_decision"));
}

#[test]
fn multibyte_labels_cannot_panic_the_label_prefix_check() {
    // Byte 7 of each label falls inside a multibyte character, so slicing at
    // byte 7 would panic. Labels come from live GraphQL, and one such label
    // on any watched pull request must not take down the whole rules pass.
    for label in ["🚧🚧", "✅✅✅", "owner🚧x"] {
        assert!(label.len() >= 7 && !label.is_char_boundary(7), "{label}");
        let mut labelled = pr(Vec::new(), Vec::new());
        labelled.labels = vec![label.to_owned()];
        let seen = holding(&input(labelled, hold_trail(10)), 100);
        assert!(seen.contains("held_unrepaired"), "{label}: {seen:?}");
        assert!(!seen.contains("parked"), "{label}: {seen:?}");
    }
    // Control: a multibyte suffix after the real prefix still parks the PR.
    let mut parked = pr(Vec::new(), Vec::new());
    parked.labels = vec!["PARKED/🚧".to_owned()];
    let seen = holding(&input(parked, hold_trail(10)), 100);
    assert!(
        seen.contains("parked") && !seen.contains("held_unrepaired"),
        "{seen:?}"
    );
}

#[test]
fn every_event_with_a_since_carries_its_age() {
    let watch = input(
        pr(Vec::new(), Vec::new()),
        vec![comment(1, 20, &format!("verdict: APPROVE head:{HEAD}"))],
    );
    let events = evaluate(&watch, t(40), &RuleConfig::default(), false);
    assert!(!events.is_empty());
    for event in &events {
        let since = event.since.expect("since");
        assert_eq!(
            event.age_secs,
            Some((t(40) - since).num_seconds()),
            "{event:?}"
        );
        assert!(event.age.is_some());
    }
}
