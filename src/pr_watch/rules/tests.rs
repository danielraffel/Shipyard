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
    ledger.prune(t(0));
    assert_eq!(ledger.seen.keys().collect::<Vec<_>>(), vec!["new"]);
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
         "issue_url": "https://api.github.com/repos/o/r/issues/7", "body": format!("READY head:{HEAD}")},
        {"id": 2, "user": {"login": "codecov[bot]"}, "created_at": "2026-10-11T12:11:00Z",
         "issue_url": "https://api.github.com/repos/o/r/issues/7", "body": "## Codecov report"},
        {"id": 3, "user": {"login": "reviewer"}, "created_at": "2026-10-11T12:12:00Z",
         "issue_url": "https://api.github.com/repos/o/r/issues/8", "body": format!("reviewed: {HEAD}")}
    ]]);
    let reviews = json!({"data": {"repository": {"p7": {"reviews": {"nodes": [
        {"databaseId": 50, "author": {"login": "reviewer"}, "state": "CHANGES_REQUESTED",
         "submittedAt": "2026-10-11T12:20:00Z", "commit": {"oid": HEAD}},
        {"databaseId": 51, "author": {"login": "reviewer"}, "state": "COMMENTED",
         "submittedAt": "2026-10-11T12:21:00Z", "commit": {"oid": HEAD}}
    ]}}}}});
    let gh = fake_gh(vec![
        ("repos/o/r/issues/comments", comments.to_string()),
        ("graphql", reviews.to_string()),
    ]);
    let trails = read_trails(&gh, REPO, &BTreeSet::from([7]), t(-60)).expect("read");
    let trail = &trails[&7];
    assert_eq!(trail.len(), 2, "{trail:?}");
    assert_eq!(trail[0].ready_head.as_deref(), Some(HEAD));
    assert!(trail[1].review);
    assert_eq!(
        trail[1].verdict.as_ref().map(|verdict| verdict.approve),
        Some(false)
    );
    assert!(!trails.contains_key(&8), "pull request 8 is not watched");
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
