//! Unit tests for the PR watch engine. Synthetic histories pin each rule at
//! its boundary; the golden replay over real responses lives in
//! `golden_tests.rs`.

use std::cell::RefCell;
use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;

use super::comment::{self, CommentAction};
use super::digest::{self, DigestOutcome, DigestPolicy};
use super::flags::{DigestRoute, FlagKind, Thresholds, evaluate};
use super::ledger::{self, Ledger, PrNow};
use super::replay::{Expectation, ReplayOptions, replay};
use super::*;

const MACOS: &str = "macos";
const LINUX: &str = "Linux (x64)";

fn t(hour: i64, minute: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 28, 0, 0, 0).unwrap()
        + Duration::hours(hour)
        + Duration::minutes(minute)
}

fn sha(tag: &str) -> String {
    format!("{tag:0<40}")
}

fn check(id: u64, name: &str, conclusion: &str, done: DateTime<Utc>, sigs: &[&str]) -> CheckFact {
    CheckFact {
        name: name.to_owned(),
        id,
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: Some(done - Duration::minutes(20)),
        completed_at: Some(done),
        signatures: sigs.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn head(tag: &str, seen: DateTime<Utc>, run_conclusion: &str, checks: Vec<CheckFact>) -> HeadFact {
    HeadFact {
        sha: sha(tag),
        first_seen_at: seen,
        gate_runs: vec![RunFact {
            id: seen.timestamp().unsigned_abs(),
            created_at: seen,
            status: "completed".to_owned(),
            conclusion: Some(run_conclusion.to_owned()),
        }],
        checks,
        merge_base: None,
    }
}

fn pr(number: u64, heads: Vec<HeadFact>, events: Vec<QueueEvent>) -> PrHistory {
    PrHistory {
        number,
        title: format!("PR {number}"),
        url: format!("https://github.com/o/r/pull/{number}"),
        created_at: Some(t(-10, 0)),
        merged_at: None,
        closed_at: None,
        changed_files: 5,
        commits: 2,
        head_ref: format!("branch-{number}"),
        head_sha: heads.last().map(|h| h.sha.clone()).unwrap_or_default(),
        labels: Vec::new(),
        heads,
        events,
        timeline_complete: true,
    }
}

fn history(prs: Vec<PrHistory>, groups: Vec<GroupRun>) -> RepoHistory {
    RepoHistory {
        repo: "o/r".to_owned(),
        base: "main".to_owned(),
        from: t(-24, 0),
        to: t(24, 0),
        required_checks: vec![MACOS.to_owned(), "Vellum freeze".to_owned()],
        prs: prs.into_iter().map(|p| (p.number, p)).collect(),
        group_runs: groups,
        gaps: Vec::new(),
    }
}

fn ev(at: DateTime<Utc>, kind: QueueEventKind) -> QueueEvent {
    QueueEvent { at, kind }
}

fn kinds(flags: &[Flag], pr: u64) -> Vec<u8> {
    flags
        .iter()
        .filter(|flag| flag.pr == pr)
        .map(|flag| flag.kind.number())
        .collect()
}

fn group(id: u64, pr: u64, at: DateTime<Utc>, failed_jobs: &[&str]) -> GroupRun {
    GroupRun {
        id,
        pr: Some(pr),
        head_sha: sha(&format!("g{id}")),
        parent_sha: Some(sha("base")),
        created_at: at - Duration::minutes(30),
        conclusion: Some(
            if failed_jobs.is_empty() {
                "success"
            } else {
                "failure"
            }
            .to_owned(),
        ),
        required_jobs: failed_jobs
            .iter()
            .enumerate()
            .map(|(i, name)| check(id * 10 + i as u64, name, "failure", at, &[]))
            .collect(),
        attribution: None,
    }
}

// ---- parsing ------------------------------------------------------------------

#[test]
fn ctest_lines_normalise_to_the_bare_test_name() {
    assert_eq!(
        normalize_ctest_name("21516 - consumption-census-drift (Failed)  pr-fast"),
        "consumption-census-drift"
    );
    assert_eq!(
        normalize_ctest_name("\t21529 - consumption-census-drift (Failed)                 pr-fast"),
        "consumption-census-drift"
    );
    assert_eq!(normalize_ctest_name("7 - slow-one (Timeout)"), "slow-one");
    assert_eq!(normalize_ctest_name("name-without-id"), "name-without-id");
}

#[test]
fn signatures_prefer_ctest_names_then_a_meaningful_error_line() {
    let ctest = "2026-09-29T03:26:54.2414940Z The following tests FAILED:\n\
                 2026-09-29T03:26:54.2415080Z \t21529 - consumption-census-drift (Failed)                 pr-fast\n\
                 2026-09-29T03:26:54.2415270Z \t21530 - consumption-census-drift-description (Failed)     pr-fast\n\
                 2026-09-29T03:26:54.2417330Z Errors while running CTest\n\
                 2026-09-29T03:26:54.2430450Z ##[error]Process completed with exit code 8.\n";
    assert_eq!(
        failure_signatures(ctest),
        vec![
            "consumption-census-drift".to_owned(),
            "consumption-census-drift-description".to_owned()
        ]
    );
    let infra = "2026-09-29T03:51:19.7413770Z ##[error]Process completed with exit code 65.\n\
                 2026-09-29T03:51:19.7413770Z ##[error]iOS compile gate failed (exit 65)\n";
    assert_eq!(
        failure_signatures(infra),
        vec!["error: iOS compile gate failed (exit 65)".to_owned()]
    );
    assert!(failure_signatures("##[error]Process completed with exit code 2.\n").is_empty());
}

// ---- flag 1 -------------------------------------------------------------------

fn repeat_history(other_prs_failing: usize, second_head_green: bool) -> RepoHistory {
    let second = if second_head_green {
        check(2, MACOS, "success", t(2, 0), &[])
    } else {
        check(2, MACOS, "failure", t(2, 0), &["census-drift"])
    };
    let mut prs = vec![pr(
        100,
        vec![
            head(
                "a1",
                t(0, 0),
                "failure",
                vec![check(1, MACOS, "failure", t(1, 0), &["census-drift"])],
            ),
            head("a2", t(1, 10), "failure", vec![second]),
        ],
        vec![],
    )];
    for index in 0..other_prs_failing {
        let number = 200 + index as u64;
        prs.push(pr(
            number,
            vec![head(
                &format!("o{index}"),
                t(0, 0),
                "failure",
                vec![check(
                    50 + index as u64,
                    MACOS,
                    "failure",
                    t(1, 30),
                    &["census-drift"],
                )],
            )],
            vec![],
        ));
    }
    history(prs, vec![])
}

#[test]
fn flag1_same_test_on_two_heads_is_a_code_failure() {
    let flags = evaluate(&repeat_history(0, false), t(2, 5), &Thresholds::default());
    let flag = flags.iter().find(|f| f.pr == 100).expect("flag 1");
    assert_eq!(flag.kind, FlagKind::RepeatTestFailure);
    assert_eq!(flag.verdict, "code failure, not flake");
    assert!(flag.evidence.contains("census-drift"), "{}", flag.evidence);
}

#[test]
fn flag1_needs_two_runs_and_a_still_red_check() {
    let thresholds = Thresholds::default();
    // Only the first failure had happened.
    assert!(
        kinds(
            &evaluate(&repeat_history(0, false), t(1, 30), &thresholds),
            100
        )
        .is_empty()
    );
    // The latest run went green.
    assert!(
        kinds(
            &evaluate(&repeat_history(0, true), t(2, 5), &thresholds),
            100
        )
        .is_empty()
    );
}

#[test]
fn flag1_is_pre_existing_when_two_other_prs_fail_the_same_test() {
    let flags = evaluate(&repeat_history(2, false), t(2, 5), &Thresholds::default());
    let flag = flags.iter().find(|f| f.pr == 100).expect("flag 1");
    assert_eq!(flag.verdict, "failing on main/pre-existing");
    assert!(flag.evidence.contains("#200") && flag.evidence.contains("#201"));
    // One other PR is not enough.
    let flags = evaluate(&repeat_history(1, false), t(2, 5), &Thresholds::default());
    assert_eq!(
        flags.iter().find(|f| f.pr == 100).unwrap().verdict,
        "code failure, not flake"
    );
}

#[test]
fn flag1_ignores_advisory_jobs() {
    let history = history(
        vec![pr(
            100,
            vec![
                head(
                    "a1",
                    t(0, 0),
                    "failure",
                    vec![check(1, LINUX, "failure", t(1, 0), &["census-drift"])],
                ),
                head(
                    "a2",
                    t(1, 10),
                    "failure",
                    vec![check(2, LINUX, "failure", t(2, 0), &["census-drift"])],
                ),
            ],
            vec![],
        )],
        vec![],
    );
    assert!(evaluate(&history, t(3, 0), &Thresholds::default()).is_empty());
}

// ---- flag 2 -------------------------------------------------------------------

fn armed_history(events: Vec<QueueEvent>, extra_head: Option<HeadFact>) -> RepoHistory {
    let mut heads = vec![head(
        "b1",
        t(0, 0),
        "failure",
        vec![check(9, MACOS, "failure", t(1, 0), &[])],
    )];
    heads.extend(extra_head);
    history(vec![pr(300, heads, events)], vec![])
}

#[test]
fn flag2_fires_only_after_the_red_threshold() {
    let history = armed_history(vec![ev(t(0, 1), QueueEventKind::Armed)], None);
    let thresholds = Thresholds::default();
    assert!(kinds(&evaluate(&history, t(1, 29), &thresholds), 300).is_empty());
    assert!(kinds(&evaluate(&history, t(1, 30), &thresholds), 300).is_empty());
    assert_eq!(
        kinds(&evaluate(&history, t(1, 31), &thresholds), 300),
        vec![2]
    );
}

#[test]
fn flag2_needs_arming_and_no_newer_head() {
    let thresholds = Thresholds::default();
    let unarmed = armed_history(vec![], None);
    assert!(kinds(&evaluate(&unarmed, t(3, 0), &thresholds), 300).is_empty());
    let disarmed = armed_history(
        vec![
            ev(t(0, 1), QueueEventKind::Armed),
            ev(t(0, 30), QueueEventKind::Disarmed { reason: None }),
        ],
        None,
    );
    assert!(kinds(&evaluate(&disarmed, t(3, 0), &thresholds), 300).is_empty());
    let queued = armed_history(
        vec![
            ev(t(0, 1), QueueEventKind::Armed),
            ev(t(0, 30), QueueEventKind::Enqueued),
        ],
        None,
    );
    assert!(kinds(&evaluate(&queued, t(3, 0), &thresholds), 300).is_empty());
    let pushed = armed_history(
        vec![ev(t(0, 1), QueueEventKind::Armed)],
        Some(head("b2", t(1, 20), "in_progress", vec![])),
    );
    assert!(kinds(&evaluate(&pushed, t(3, 0), &thresholds), 300).is_empty());
}

#[test]
fn flag2_counts_an_ejection_for_failed_checks_as_armed() {
    let thresholds = Thresholds::default();
    let ejected = armed_history(
        vec![
            ev(t(0, 1), QueueEventKind::Armed),
            ev(t(0, 10), QueueEventKind::Enqueued),
            ev(
                t(0, 40),
                QueueEventKind::Removed {
                    reason: "failed_checks".to_owned(),
                },
            ),
        ],
        None,
    );
    assert_eq!(
        kinds(&evaluate(&ejected, t(2, 0), &thresholds), 300),
        vec![2]
    );
    let manual = armed_history(
        vec![
            ev(t(0, 1), QueueEventKind::Armed),
            ev(t(0, 10), QueueEventKind::Enqueued),
            ev(
                t(0, 40),
                QueueEventKind::Removed {
                    reason: "manual".to_owned(),
                },
            ),
        ],
        None,
    );
    assert!(kinds(&evaluate(&manual, t(2, 0), &thresholds), 300).is_empty());
}

// ---- flag 3 -------------------------------------------------------------------

#[test]
fn flag3_needs_two_failed_named_groups_on_a_required_job() {
    let thresholds = Thresholds::default();
    let base = pr(
        400,
        vec![head(
            "c1",
            t(0, 0),
            "success",
            vec![check(1, MACOS, "success", t(0, 30), &[])],
        )],
        vec![],
    );
    let one = history(vec![base.clone()], vec![group(1, 400, t(2, 0), &[MACOS])]);
    assert!(kinds(&evaluate(&one, t(5, 0), &thresholds), 400).is_empty());
    let two = history(
        vec![base.clone()],
        vec![
            group(1, 400, t(2, 0), &[MACOS]),
            group(2, 400, t(3, 0), &[MACOS]),
        ],
    );
    assert_eq!(kinds(&evaluate(&two, t(3, 5), &thresholds), 400), vec![3]);
    assert!(kinds(&evaluate(&two, t(2, 30), &thresholds), 400).is_empty());
    let advisory = history(
        vec![base.clone()],
        vec![
            group(1, 400, t(2, 0), &[LINUX]),
            group(2, 400, t(3, 0), &[LINUX]),
        ],
    );
    assert!(kinds(&evaluate(&advisory, t(5, 0), &thresholds), 400).is_empty());
    let recovered = history(
        vec![base],
        vec![
            group(1, 400, t(2, 0), &[MACOS]),
            group(2, 400, t(3, 0), &[MACOS]),
            group(3, 400, t(4, 0), &[]),
        ],
    );
    assert!(kinds(&evaluate(&recovered, t(5, 0), &thresholds), 400).is_empty());
}

// ---- flag 4 -------------------------------------------------------------------

fn treadmill(replacements: usize, same_base: bool) -> RepoHistory {
    let mut heads = Vec::new();
    for index in 0..=replacements {
        let mut next = head(
            &format!("d{index}"),
            t(i64::try_from(index).unwrap(), 0),
            if index == replacements {
                "in_progress"
            } else {
                "cancelled"
            },
            vec![],
        );
        next.merge_base = Some(if same_base {
            sha("m")
        } else {
            sha(&format!("m{index}"))
        });
        heads.push(next);
    }
    history(vec![pr(500, heads, vec![])], vec![])
}

#[test]
fn flag4_needs_three_cancelled_replacements_with_a_moving_base() {
    let thresholds = Thresholds::default();
    assert_eq!(
        kinds(&evaluate(&treadmill(3, false), t(4, 0), &thresholds), 500),
        vec![4]
    );
    assert!(kinds(&evaluate(&treadmill(2, false), t(4, 0), &thresholds), 500).is_empty());
    assert!(kinds(&evaluate(&treadmill(3, true), t(4, 0), &thresholds), 500).is_empty());
    // Outside the 24 h window it expires.
    assert!(kinds(&evaluate(&treadmill(3, false), t(26, 0), &thresholds), 500).is_empty());
    let flags = evaluate(&treadmill(3, false), t(4, 0), &thresholds);
    assert!(
        flags[0].evidence.contains("inferred"),
        "{}",
        flags[0].evidence
    );
}

#[test]
fn flag4_ignores_replacements_whose_gate_was_not_cancelled() {
    let mut history = treadmill(3, false);
    for head in &mut history.prs.get_mut(&500).unwrap().heads {
        head.gate_runs[0].conclusion = Some("failure".to_owned());
    }
    assert!(kinds(&evaluate(&history, t(4, 0), &Thresholds::default()), 500).is_empty());
}

// ---- flag 5 -------------------------------------------------------------------

#[test]
fn flag5_is_advisory_and_never_alone() {
    let mut big = pr(600, vec![head("e1", t(0, 0), "success", vec![])], vec![]);
    big.changed_files = 131;
    big.commits = 77;
    big.created_at = Some(t(-100, 0));
    let alone = history(vec![big.clone()], vec![]);
    assert!(evaluate(&alone, t(3, 0), &Thresholds::default()).is_empty());
    let with_other = history(
        vec![big],
        vec![
            group(1, 600, t(1, 0), &[MACOS]),
            group(2, 600, t(2, 0), &[MACOS]),
        ],
    );
    assert_eq!(
        kinds(&evaluate(&with_other, t(3, 0), &Thresholds::default()), 600),
        vec![3, 5]
    );
}

#[test]
fn closed_or_merged_prs_raise_nothing() {
    let mut history = armed_history(vec![ev(t(0, 1), QueueEventKind::Armed)], None);
    history.prs.get_mut(&300).unwrap().closed_at = Some(t(1, 45));
    assert!(evaluate(&history, t(2, 0), &Thresholds::default()).is_empty());
}

// ---- ledger + digest ------------------------------------------------------------

fn now_map(pr: u64, head: &str, open: bool) -> BTreeMap<u64, PrNow> {
    BTreeMap::from([(
        pr,
        PrNow {
            open,
            merged: !open,
            head_sha: head.to_owned(),
            acknowledged: false,
            title: "t".to_owned(),
            url: "u".to_owned(),
        },
    )])
}

fn flag(pr: u64, kind: FlagKind, head: &str) -> Flag {
    Flag {
        pr,
        kind,
        key: String::new(),
        verdict: "v".to_owned(),
        evidence: "e".to_owned(),
        head_sha: head.to_owned(),
        route: DigestRoute::PerPr,
        shared_tests: Vec::new(),
        related_prs: Vec::new(),
    }
}

#[test]
fn ledger_keeps_first_seen_and_restarts_on_a_new_head() {
    let mut ledger = Ledger::new("o/r", "main");
    let f = flag(1, FlagKind::RepeatedEjection, "h1");
    ledger::reconcile(
        &mut ledger,
        std::slice::from_ref(&f),
        &now_map(1, "h1", true),
        t(0, 0),
    );
    ledger::reconcile(
        &mut ledger,
        std::slice::from_ref(&f),
        &now_map(1, "h1", true),
        t(1, 0),
    );
    let entry = ledger.entries.values().next().unwrap();
    assert_eq!(entry.first_seen_at, t(0, 0));
    assert_eq!(entry.last_seen_at, t(1, 0));
    let moved = flag(1, FlagKind::RepeatedEjection, "h2");
    ledger::reconcile(&mut ledger, &[moved], &now_map(1, "h2", true), t(2, 0));
    assert_eq!(
        ledger.entries.values().next().unwrap().first_seen_at,
        t(2, 0)
    );
    let events = ledger::reconcile(&mut ledger, &[], &now_map(1, "h2", true), t(3, 0));
    let entry = ledger.entries.values().next().unwrap();
    assert_eq!(entry.addressed_reason.as_deref(), Some("cleared"));
    assert_eq!(events[0].change, "addressed:cleared");
}

#[test]
fn a_new_head_does_not_restart_a_treadmill_flag() {
    let mut ledger = Ledger::new("o/r", "main");
    ledger::reconcile(
        &mut ledger,
        &[flag(1, FlagKind::RebaseTreadmill, "h1")],
        &now_map(1, "h1", true),
        t(0, 0),
    );
    ledger::reconcile(
        &mut ledger,
        &[flag(1, FlagKind::RebaseTreadmill, "h2")],
        &now_map(1, "h2", true),
        t(1, 0),
    );
    let entry = ledger.entries.values().next().unwrap();
    assert_eq!(entry.first_seen_at, t(0, 0));
    assert_eq!(entry.head_sha, "h2");
}

#[test]
fn an_ack_label_addresses_the_flag() {
    let mut ledger = Ledger::new("o/r", "main");
    let mut prs = now_map(1, "h1", true);
    prs.get_mut(&1).unwrap().acknowledged = true;
    ledger::reconcile(
        &mut ledger,
        &[flag(1, FlagKind::RedWhileArmed, "h1")],
        &prs,
        t(0, 0),
    );
    assert_eq!(
        ledger
            .entries
            .values()
            .next()
            .unwrap()
            .addressed_reason
            .as_deref(),
        Some("ack_label")
    );
    assert!(digest::select(&ledger, t(5, 0), DigestPolicy::default()).is_none());
}

#[test]
fn ledger_round_trips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pr-watch").join("ledger.json");
    let mut ledger = Ledger::new("o/r", "main");
    ledger::reconcile(
        &mut ledger,
        &[flag(1, FlagKind::RedWhileArmed, "h1")],
        &now_map(1, "h1", true),
        t(0, 0),
    );
    let _lock = ledger::lock(&path).unwrap();
    ledger::save(&path, &ledger).unwrap();
    assert_eq!(ledger::load(&path, "o/r", "main").unwrap(), ledger);
    assert!(ledger::load(&path, "o/other", "main").is_err());
    assert_eq!(
        ledger::load(&dir.path().join("missing.json"), "o/r", "main").unwrap(),
        Ledger::new("o/r", "main")
    );
}

#[test]
fn digest_carries_only_aged_unaddressed_flags_and_never_split_alone() {
    let mut ledger = Ledger::new("o/r", "main");
    let flags = [
        flag(1, FlagKind::RepeatedEjection, "h1"),
        flag(1, FlagKind::SplitCandidate, "h1"),
    ];
    ledger::reconcile(&mut ledger, &flags, &now_map(1, "h1", true), t(0, 0));
    let policy = DigestPolicy::default();
    assert!(
        digest::select(&ledger, t(1, 59), policy).is_none(),
        "younger than 2 h"
    );
    let selection = digest::select(&ledger, t(2, 0), policy).expect("aged in");
    assert_eq!(
        selection.ids().len(),
        2,
        "split rides along with another flag on the PR"
    );
    assert_eq!(selection.per_pr.len(), 1, "one line per PR");
    // A split flag alone never reaches a digest.
    let mut alone = Ledger::new("o/r", "main");
    ledger::reconcile(
        &mut alone,
        &[flag(2, FlagKind::SplitCandidate, "h")],
        &now_map(2, "h", true),
        t(0, 0),
    );
    assert!(digest::select(&alone, t(5, 0), policy).is_none());
}

#[test]
fn digest_is_claimed_then_sent_and_rolled_back_on_failure() {
    let mut ledger = Ledger::new("o/r", "main");
    ledger::reconcile(
        &mut ledger,
        &[flag(1, FlagKind::RedWhileArmed, "h")],
        &now_map(1, "h", true),
        t(0, 0),
    );
    let policy = DigestPolicy::default();
    let saved = RefCell::new(Vec::<Ledger>::new());
    let mut persist = |l: &Ledger| {
        saved.borrow_mut().push(l.clone());
        Ok(())
    };
    let mut failing = |_: &str| Err("webhook down".to_owned());
    let error = digest::run(
        &mut ledger,
        t(3, 0),
        policy,
        true,
        &mut persist,
        &mut failing,
    )
    .unwrap_err();
    assert!(error.contains("rolled back"));
    assert!(
        saved.borrow()[0].digest_claim.is_some(),
        "claimed before sending"
    );
    assert!(ledger.digest_claim.is_none() && ledger.last_digest_at.is_none());
    let sent = RefCell::new(String::new());
    let mut ok = |payload: &str| {
        *sent.borrow_mut() = payload.to_owned();
        Ok(())
    };
    let (outcome, _) =
        digest::run(&mut ledger, t(3, 15), policy, true, &mut persist, &mut ok).unwrap();
    assert_eq!(outcome, DigestOutcome::Sent { lines: 1 });
    let payload: serde_json::Value = serde_json::from_str(&sent.borrow()).unwrap();
    assert_eq!(payload["schema"], "shipyard.pr-watch.digest/v1");
    assert_eq!(payload["repo"], "o/r");
    assert_eq!(payload["window"]["min_age_minutes"], 120);
    assert_eq!(payload["flags"][0]["kind"], "red_while_armed");
    assert_eq!(payload["flags"][0]["age_minutes"], 195);
    // Within the hour, and once sent: skipped.
    let (outcome, _) =
        digest::run(&mut ledger, t(3, 30), policy, true, &mut persist, &mut ok).unwrap();
    assert_eq!(outcome, DigestOutcome::Skipped);
    let (outcome, _) =
        digest::run(&mut ledger, t(5, 0), policy, true, &mut persist, &mut ok).unwrap();
    assert_eq!(
        outcome,
        DigestOutcome::Skipped,
        "a flag is sent once per episode"
    );
}

#[test]
fn a_stale_claim_counts_as_delivered() {
    let mut ledger = Ledger::new("o/r", "main");
    ledger::reconcile(
        &mut ledger,
        &[flag(1, FlagKind::RedWhileArmed, "h")],
        &now_map(1, "h", true),
        t(0, 0),
    );
    let id = ledger.entries.keys().next().unwrap().clone();
    ledger.digest_claim = Some(ledger::DigestClaim {
        claimed_at: t(2, 0),
        ids: vec![id],
        shared: Vec::new(),
    });
    let mut persist = |_: &Ledger| Ok(());
    let mut never = |_: &str| -> Result<(), String> { panic!("must not resend") };
    let (outcome, _) = digest::run(
        &mut ledger,
        t(4, 0),
        DigestPolicy::default(),
        true,
        &mut persist,
        &mut never,
    )
    .unwrap();
    assert_eq!(outcome, DigestOutcome::Skipped);
    assert_eq!(ledger.last_digest_at, Some(t(2, 0)));
}

// ---- sticky comment -------------------------------------------------------------

#[test]
fn sticky_comment_patches_only_on_change_and_ignores_foreign_markers() {
    let flags = [flag(7, FlagKind::RepeatedEjection, "h")];
    let comments = json!([[
        {"id": 11, "body": format!("{COMMENT_MARKER}\nold"), "user": {"login": "someone-else"}},
    ]])
    .to_string();
    let reader = |argv: &[String]| -> Result<String, String> {
        assert!(argv.iter().any(|a| a.ends_with("issues/7/comments")));
        Ok(comments.clone())
    };
    let mut ledger = Ledger::new("o/r", "main");
    let (actions, gaps) = comment::plan(
        &reader,
        &mut ledger,
        &flags,
        &[7],
        Some("shipyard-local[bot]"),
    );
    assert!(gaps.is_empty());
    assert!(
        matches!(actions.as_slice(), [CommentAction::Create { pr: 7, .. }]),
        "{actions:?}"
    );
    let sent_writes = RefCell::new(Vec::new());
    let writer = |argv: &[String]| {
        sent_writes.borrow_mut().push(argv.to_vec());
        Ok(json!({"id": 99}).to_string())
    };
    assert!(comment::apply(&mut ledger, &actions, &writer).is_empty());
    assert_eq!(ledger.comments[&7].comment_id, 99);
    // Same body again: nothing to send, and no read either.
    let no_read =
        |_: &[String]| -> Result<String, String> { panic!("recorded comment needs no read") };
    let (again, _) = comment::plan(&no_read, &mut ledger, &flags, &[7], None);
    assert!(again.is_empty());
    // All clear: rewrite to resolved.
    let (resolved, _) = comment::plan(&no_read, &mut ledger, &[], &[7], None);
    match resolved.as_slice() {
        [
            CommentAction::Update {
                comment_id: 99,
                body,
                ..
            },
        ] => assert!(body.contains("resolved")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn our_existing_comment_is_adopted_by_author_and_marker() {
    let flags = [flag(7, FlagKind::RepeatedEjection, "h")];
    let comments = json!([[
        {"id": 12, "body": format!("{COMMENT_MARKER}\nold"), "user": {"login": "shipyard-local[bot]"}},
    ]])
    .to_string();
    let reader = |_: &[String]| -> Result<String, String> { Ok(comments.clone()) };
    let mut ledger = Ledger::new("o/r", "main");
    let (actions, _) = comment::plan(
        &reader,
        &mut ledger,
        &flags,
        &[7],
        Some("shipyard-local[bot]"),
    );
    assert!(
        matches!(
            actions.as_slice(),
            [CommentAction::Update { comment_id: 12, .. }]
        ),
        "{actions:?}"
    );
}

#[test]
fn an_unreadable_comment_list_is_unknown_not_absent() {
    let reader = |_: &[String]| -> Result<String, String> { Err("HTTP 502".to_owned()) };
    let mut ledger = Ledger::new("o/r", "main");
    let (actions, gaps) = comment::plan(
        &reader,
        &mut ledger,
        &[flag(7, FlagKind::RepeatedEjection, "h")],
        &[7],
        Some("bot"),
    );
    assert!(actions.is_empty());
    assert_eq!(gaps.len(), 1);
}

// ---- scan: the read-only guard ------------------------------------------------------

/// Synthetic GitHub for one open pull request that is red while armed.
fn fake_github(argv: &[String]) -> Result<String, String> {
    let path = argv
        .iter()
        .find(|a| a.starts_with("repos/"))
        .cloned()
        .unwrap_or_default();
    if argv.get(1).map(String::as_str) == Some("graphql") {
        return Ok(json!({"data": {"search": {"issueCount": 1,
            "pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [{
            "number": 42, "title": "t", "url": "https://github.com/o/r/pull/42", "state": "OPEN",
            "createdAt": "2026-09-27T00:00:00Z", "mergedAt": null, "closedAt": null,
            "headRefName": "feature/x", "headRefOid": sha("f1"), "changedFiles": 3,
            "commits": {"totalCount": 1}, "labels": {"nodes": []},
            "timelineItems": {"pageInfo": {"hasPreviousPage": false}, "nodes": [
                {"__typename": "AutoMergeEnabledEvent", "createdAt": "2026-09-28T00:00:00Z"}
            ]}
        }]}}})
        .to_string());
    }
    if path.ends_with("/branches/main") {
        return Ok(
            json!({"protection": {"required_status_checks": {"contexts": ["macos"]}}}).to_string(),
        );
    }
    if path.contains("/actions/workflows/") {
        let pull_request = argv.iter().any(|a| a == "event=pull_request");
        let first_day = argv.iter().any(|a| a.starts_with("created=2026-09-27T12"));
        let runs = if pull_request && first_day {
            json!([{"id": 1, "head_branch": "feature/x", "head_sha": sha("f1"), "status": "completed",
                    "conclusion": "failure", "created_at": "2026-09-28T00:05:00Z", "run_attempt": 1}])
        } else {
            json!([])
        };
        let total = runs.as_array().map_or(0, Vec::len);
        return Ok(json!([{"total_count": total, "workflow_runs": runs}]).to_string());
    }
    if path.contains("/check-runs") {
        return Ok(json!([{"total_count": 2, "check_runs": [
            {"id": 77, "name": "macos", "status": "completed", "conclusion": "failure",
             "started_at": "2026-09-28T00:10:00Z", "completed_at": "2026-09-28T00:40:00Z"},
            {"id": 78, "name": "Linux (x64)", "status": "completed", "conclusion": "failure",
             "started_at": "2026-09-28T00:10:00Z", "completed_at": "2026-09-28T00:40:00Z"}
        ]}])
        .to_string());
    }
    if path.ends_with("/logs") {
        return Ok("##[error]boom\n".to_owned());
    }
    if path.ends_with("/issues/42/comments") {
        return Ok(json!([[]]).to_string());
    }
    Err(format!("unexpected request {argv:?}"))
}

/// Whether an argv could change anything on GitHub: any non-GET method, or
/// field flags on a REST call not explicitly pinned to GET. GraphQL requests
/// carry only a query document, never a mutation (asserted separately).
fn is_mutating(argv: &[String]) -> bool {
    let method = argv
        .windows(2)
        .find(|pair| matches!(pair[0].as_str(), "--method" | "-X"))
        .map(|pair| pair[1].to_ascii_uppercase());
    if argv.get(1).map(String::as_str) == Some("graphql") {
        return argv.iter().any(|a| {
            a.starts_with("query=")
                && a.trim_start_matches("query=")
                    .trim_start()
                    .starts_with("mutation")
        });
    }
    match method.as_deref() {
        Some("GET") | None if !argv.iter().any(|a| a == "-f" || a == "-F") => false,
        Some("GET") => false,
        _ => true,
    }
}

type ScanRun = (Vec<Vec<String>>, Vec<Vec<String>>, super::scan::ScanReport);

fn run_scan(post_comments: bool) -> ScanRun {
    let dir = tempfile::tempdir().unwrap();
    let reads = std::sync::Mutex::new(Vec::new());
    let reader = |argv: &[String]| {
        reads.lock().unwrap().push(argv.to_vec());
        fake_github(argv)
    };
    let sent_writes = RefCell::new(Vec::new());
    let writer = |argv: &[String]| {
        sent_writes.borrow_mut().push(argv.to_vec());
        Ok(json!({"id": 5}).to_string())
    };
    let mut sender = |_: &str| -> Result<(), String> { panic!("digest not requested") };
    let config = super::scan::WatchConfig {
        lookback: Duration::days(2),
        comment_author: Some("shipyard-local[bot]".to_owned()),
        ..super::scan::WatchConfig::default()
    };
    let request = super::scan::ScanRequest {
        repo: "o/r".to_owned(),
        config,
        state_path: dir.path().join("ledger.json"),
        post_comments,
        post_digest: false,
        plan_comments: true,
        handback: super::handback::HandbackMode::Off,
    };
    let now = Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();
    let report = super::scan::scan(
        &reader,
        &writer,
        &mut sender,
        None,
        &crate::gate_cost::ReadCache::disabled(),
        &request,
        now,
        None,
    )
    .unwrap();
    let reads = reads.into_inner().unwrap();
    (reads, sent_writes.into_inner(), report)
}

#[test]
fn scan_sends_no_mutating_request_without_post_comments() {
    let (reads, writes, report) = run_scan(false);
    assert_eq!(kinds(&report.flags, 42), vec![2], "{:?}", report.flags);
    assert!(writes.is_empty(), "dry run wrote {writes:?}");
    assert!(!reads.is_empty());
    for argv in &reads {
        assert!(
            !is_mutating(argv),
            "read path produced a mutating argv: {argv:?}"
        );
    }
    assert_eq!(
        report.comment_actions.len(),
        1,
        "the dry run still shows the comment"
    );
}

#[test]
fn post_comments_only_ever_reaches_comment_endpoints() {
    let (reads, writes, _) = run_scan(true);
    assert_eq!(writes.len(), 1);
    for argv in &writes {
        let method = argv
            .windows(2)
            .find(|p| p[0] == "--method")
            .map(|p| p[1].clone());
        let path = argv
            .iter()
            .find(|a| a.starts_with("repos/"))
            .cloned()
            .unwrap_or_default();
        let ok = (method.as_deref() == Some("POST") && path == "repos/o/r/issues/42/comments")
            || (method.as_deref() == Some("PATCH")
                && path.starts_with("repos/o/r/issues/comments/"));
        assert!(ok, "non-comment mutation {argv:?}");
    }
    for argv in &reads {
        assert!(!is_mutating(argv), "{argv:?}");
    }
}

#[test]
fn the_guard_recognises_a_mutation() {
    let post = CommentAction::Create {
        pr: 1,
        body: "b".to_owned(),
    }
    .argv("o/r");
    assert!(is_mutating(&post));
    assert!(is_mutating(&[
        "api".to_owned(),
        "-X".to_owned(),
        "PUT".to_owned(),
        "repos/o/r/pulls/1/merge".to_owned(),
    ]));
    assert!(!is_mutating(&[
        "api".to_owned(),
        "repos/o/r/branches/main".to_owned(),
    ]));
}

// ---- replay -------------------------------------------------------------------

#[test]
fn replay_uses_the_same_rules_and_checks_expectations_and_the_control() {
    let mut clean = pr(
        700,
        vec![head(
            "z1",
            t(0, 0),
            "success",
            vec![check(3, MACOS, "success", t(0, 30), &[])],
        )],
        vec![],
    );
    clean.merged_at = Some(t(1, 0));
    clean.closed_at = Some(t(1, 0));
    let mut history = armed_history(vec![ev(t(0, 1), QueueEventKind::Armed)], None);
    history.prs.insert(700, clean);
    history.from = t(0, 0);
    history.to = t(6, 0);
    let options = ReplayOptions {
        tick: Duration::minutes(15),
        thresholds: Thresholds::default(),
        digest: DigestPolicy::default(),
        expectations: vec!["300=2".parse::<Expectation>().unwrap()],
        control_merged_clean: true,
    };
    let report = replay(&history, &options);
    assert!(report.pass, "{report:#?}");
    assert_eq!(report.control.as_ref().unwrap().clean_merged, vec![700]);
    let episode = &report.episodes[0];
    assert_eq!((episode.pr, episode.flag), (300, 2));
    assert_eq!(
        episode.first_seen_at,
        t(1, 45),
        "first tick past 30 min red"
    );
    assert_eq!(episode.digested_at, Some(t(3, 45)), "digest after 2 h");
    let failing = ReplayOptions {
        expectations: vec!["300=1".parse().unwrap()],
        ..options
    };
    assert!(!replay(&history, &failing).pass);
}

#[test]
fn expectations_parse_and_reject_nonsense() {
    let parsed: Expectation = "8933=1,3,4,5".parse().unwrap();
    assert_eq!(parsed.pr, 8933);
    assert_eq!(parsed.kinds.len(), 4);
    assert!("8933".parse::<Expectation>().is_err());
    assert!("8933=9".parse::<Expectation>().is_err());
}

#[test]
fn a_daemon_style_scan_without_posting_reads_no_comment_lists() {
    let dir = tempfile::tempdir().unwrap();
    let reads = std::sync::Mutex::new(Vec::new());
    let reader = |argv: &[String]| {
        reads.lock().unwrap().push(argv.to_vec());
        fake_github(argv)
    };
    let writer = |_: &[String]| -> Result<String, String> { panic!("no writes") };
    let mut sender = |_: &str| -> Result<(), String> { panic!("no digest") };
    let request = super::scan::ScanRequest {
        repo: "o/r".to_owned(),
        config: super::scan::WatchConfig {
            lookback: Duration::days(2),
            ..super::scan::WatchConfig::default()
        },
        state_path: dir.path().join("ledger.json"),
        post_comments: false,
        post_digest: false,
        plan_comments: false,
        handback: super::handback::HandbackMode::Off,
    };
    let now = Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();
    let report = super::scan::scan(
        &reader,
        &writer,
        &mut sender,
        None,
        &crate::gate_cost::ReadCache::disabled(),
        &request,
        now,
        None,
    )
    .unwrap();
    assert_eq!(kinds(&report.flags, 42), vec![2]);
    assert!(report.comment_actions.is_empty());
    assert!(
        !reads
            .into_inner()
            .unwrap()
            .iter()
            .any(|argv| argv.iter().any(|a| a.ends_with("/comments")))
    );
}

// ---- digest grouping, shared failures, attribution -------------------------------

fn routed(pr: u64, kind: FlagKind, route: DigestRoute, tests: &[&str], related: &[u64]) -> Flag {
    Flag {
        key: if kind == FlagKind::RepeatTestFailure {
            MACOS.to_owned()
        } else {
            String::new()
        },
        route,
        shared_tests: tests.iter().map(|t| (*t).to_owned()).collect(),
        related_prs: related.to_vec(),
        ..flag(pr, kind, "h")
    }
}

fn open_prs(numbers: &[u64]) -> BTreeMap<u64, PrNow> {
    numbers
        .iter()
        .flat_map(|pr| now_map(*pr, "h", true))
        .collect()
}

#[test]
fn the_digest_has_one_line_per_pr_naming_its_most_severe_flag() {
    let mut ledger = Ledger::new("o/r", "main");
    let flags = [
        flag(1, FlagKind::RepeatedEjection, "h"),
        flag(1, FlagKind::RedWhileArmed, "h"),
        flag(1, FlagKind::SplitCandidate, "h"),
        flag(2, FlagKind::RebaseTreadmill, "h"),
    ];
    ledger::reconcile(&mut ledger, &flags, &open_prs(&[1, 2]), t(0, 0));
    let policy = DigestPolicy::default();
    let selection = digest::select(&ledger, t(2, 0), policy).unwrap();
    let payload = digest::payload(&ledger, &selection, t(2, 0), policy);
    assert_eq!(payload.flags.len(), 2, "one line per PR");
    let first = &payload.flags[0];
    assert_eq!(
        (first.pr, first.kind.as_str(), first.count),
        (1, "red_while_armed", 3)
    );
    assert_eq!(
        first.kinds,
        ["red_while_armed", "repeated_ejection", "split_candidate"]
    );
    assert_eq!(payload.lines(), 2);
}

#[test]
fn shared_failures_leave_the_per_pr_lines_and_are_announced_once_per_test() {
    let mut ledger = Ledger::new("o/r", "main");
    let flags = [
        routed(
            1,
            FlagKind::RepeatTestFailure,
            DigestRoute::Shared,
            &["census-drift"],
            &[2, 3],
        ),
        routed(
            2,
            FlagKind::RepeatTestFailure,
            DigestRoute::Shared,
            &["census-drift"],
            &[1],
        ),
        routed(
            3,
            FlagKind::RepeatTestFailure,
            DigestRoute::Shared,
            &["census-drift", "other"],
            &[1],
        ),
    ];
    ledger::reconcile(&mut ledger, &flags, &open_prs(&[1, 2, 3]), t(0, 0));
    let policy = DigestPolicy::default();
    let mut persist = |_: &Ledger| Ok(());
    let sent = RefCell::new(Vec::<String>::new());
    let mut deliver = |payload: &str| {
        sent.borrow_mut().push(payload.to_owned());
        Ok(())
    };
    let (outcome, payload) = digest::run(
        &mut ledger,
        t(2, 0),
        policy,
        true,
        &mut persist,
        &mut deliver,
    )
    .unwrap();
    let payload = payload.unwrap();
    assert_eq!(outcome, DigestOutcome::Sent { lines: 2 });
    assert!(
        payload.flags.is_empty(),
        "shared failures are not owner lines"
    );
    let census = payload
        .shared_failures
        .iter()
        .find(|line| line.test == "census-drift")
        .unwrap();
    assert_eq!(census.prs, [1, 2, 3]);
    assert!(
        census.evidence.contains("likely main/cross-PR"),
        "{}",
        census.evidence
    );
    // A new PR failing the same test within 24 h: not re-announced.
    ledger::reconcile(
        &mut ledger,
        &[
            flags[0].clone(),
            flags[1].clone(),
            flags[2].clone(),
            routed(
                4,
                FlagKind::RepeatTestFailure,
                DigestRoute::Shared,
                &["census-drift"],
                &[1],
            ),
        ],
        &open_prs(&[1, 2, 3, 4]),
        t(3, 0),
    );
    assert!(digest::select(&ledger, t(6, 0), policy).is_none());
    // A day later it may be announced again.
    assert!(digest::select(&ledger, t(27, 0), policy).is_some());
}

#[test]
fn comment_only_flags_never_reach_the_digest() {
    let mut ledger = Ledger::new("o/r", "main");
    ledger::reconcile(
        &mut ledger,
        &[routed(
            1,
            FlagKind::RepeatedEjection,
            DigestRoute::CommentOnly,
            &[],
            &[],
        )],
        &open_prs(&[1]),
        t(0, 0),
    );
    assert!(digest::select(&ledger, t(5, 0), DigestPolicy::default()).is_none());
}

#[test]
fn an_attributor_that_blames_a_neighbour_downgrades_flag3_to_comment_only() {
    let base = pr(
        400,
        vec![head(
            "c1",
            t(0, 0),
            "success",
            vec![check(1, MACOS, "success", t(0, 30), &[])],
        )],
        vec![],
    );
    let neighbour = Attribution {
        verdict: "other_pull_request".to_owned(),
        implicates_head: Some(false),
        implicated_pr: Some(399),
    };
    let mut groups = vec![
        group(1, 400, t(2, 0), &[MACOS]),
        group(2, 400, t(3, 0), &[MACOS]),
    ];
    groups[0].attribution = Some(neighbour.clone());
    let one_cleared = history(vec![base.clone()], groups.clone());
    let flag3 = |h: &RepoHistory| {
        evaluate(h, t(3, 5), &Thresholds::default())
            .into_iter()
            .find(|f| f.kind == FlagKind::RepeatedEjection)
            .unwrap()
    };
    assert_eq!(
        flag3(&one_cleared).route,
        DigestRoute::PerPr,
        "every group must be cleared"
    );
    groups[1].attribution = Some(neighbour);
    let cleared = flag3(&history(vec![base], groups));
    assert_eq!(cleared.route, DigestRoute::CommentOnly);
    assert_eq!(cleared.verdict, "neighbour of #399");
    let body = comment::render(&[&cleared]).unwrap();
    assert!(body.contains("neighbour of #399"), "{body}");
}

#[test]
fn attribution_clears_only_on_positive_evidence() {
    let parse = |text: &str| Attribution::parse(text).unwrap();
    assert!(
        parse(r#"{"verdict":"other_pull_request","implicates_head":false,"implicated_pr":7}"#)
            .clears(8)
    );
    assert!(parse(r#"{"verdict":"infrastructure","implicates_head":false}"#).clears(8));
    assert!(
        !parse(r#"{"verdict":"other_pull_request","implicates_head":false,"implicated_pr":8}"#)
            .clears(8)
    );
    assert!(!parse(r#"{"verdict":"unexplained","implicates_head":null}"#).clears(8));
    assert!(!parse(r#"{"verdict":"implicates_head","implicates_head":true}"#).clears(8));
    assert!(Attribution::parse("not json").is_none());
}

#[cfg(unix)]
#[test]
fn the_configured_attributor_runs_from_the_checkout_with_the_guards_argv() {
    let root = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".shipyard")).unwrap();
    std::fs::write(
        root.path().join(".shipyard/config.toml"),
        "[queue.attribution]\ncommand = [\"sh\", \"attr.sh\"]\n",
    )
    .unwrap();
    let load = || {
        crate::config::LoadedConfig::load(
            Some(global.path().to_path_buf()),
            Some(root.path().join(".shipyard")),
            None,
            crate::config::LocalOverlaySource::None,
        )
        .unwrap()
    };
    // The script is absent: no attributor, not an error.
    assert!(super::scan::AttributorCommand::discover(&load(), root.path()).is_none());
    std::fs::write(
        root.path().join("attr.sh"),
        "[ \"$1 $3 $5\" = \"--repo --pr --run-id\" ] || exit 3\n\
         printf '{\"run_id\": 99, \"pr\": %s, \"verdict\": \"other_pull_request\", \
         \"implicates_head\": false, \"implicated_pr\": 7}' \"$4\"\n",
    )
    .unwrap();
    let command = super::scan::AttributorCommand::discover(&load(), root.path()).unwrap();
    let found = command.ask("o/r", 42, 99).unwrap();
    assert!(found.clears(42));
    assert_eq!(found.implicated_pr, Some(7));
    // A verdict about another run is not a ruling on this one.
    assert!(command.ask("o/r", 42, 100).is_none());
}

#[test]
fn a_planned_handback_rides_the_scan_and_writes_nothing() {
    struct NoHost;
    impl super::handback::host::HostRunner for NoHost {
        fn run(
            &mut self,
            invocation: &super::handback::host::Invocation,
        ) -> Result<String, super::handback::host::RunError> {
            panic!("no owner, so no host command: {:?}", invocation.argv)
        }
        fn append_local_inbox(&mut self, _: &str, _: &str) -> Result<(), String> {
            panic!("no inbox in a plan")
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let reader = |argv: &[String]| fake_github(argv);
    let writer = |argv: &[String]| -> Result<String, String> { panic!("wrote {argv:?}") };
    let mut sender = |_: &str| -> Result<(), String> { panic!("no digest") };
    let request = super::scan::ScanRequest {
        repo: "o/r".to_owned(),
        config: super::scan::WatchConfig {
            lookback: Duration::days(2),
            ..super::scan::WatchConfig::default()
        },
        state_path: dir.path().join("ledger.json"),
        post_comments: false,
        post_digest: false,
        plan_comments: false,
        handback: super::handback::HandbackMode::Plan,
    };
    let mut runner = NoHost;
    let mut deps = super::handback::Deps {
        runner: &mut runner,
        state_dir: dir.path().to_path_buf(),
        local_names: Vec::new(),
        local_machine: None,
    };
    let now = Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();
    let report = super::scan::scan(
        &reader,
        &writer,
        &mut sender,
        None,
        &crate::gate_cost::ReadCache::disabled(),
        &request,
        now,
        Some(&mut deps),
    )
    .unwrap();
    let handback = report.handback.expect("hand-back report");
    assert_eq!(handback.owners.len(), 1);
    assert_eq!(handback.owners[0].pr, 42);
    assert_eq!(handback.owners[0].record.state, "none");
    assert!(handback.actions.iter().all(|action| !action.sent));
}

/// Load `body` as the only (machine-global) config layer, as the daemon does.
fn watch_config_from_toml(body: &str) -> super::scan::WatchConfig {
    let global = tempfile::tempdir().unwrap();
    std::fs::write(global.path().join("config.toml"), body).unwrap();
    let config =
        crate::config::LoadedConfig::load_machine_global_from_dir(global.path().to_path_buf())
            .unwrap();
    super::scan::WatchConfig::from_config(&config).unwrap()
}

#[test]
fn a_digest_table_with_enabled_true_turns_the_digest_on() {
    let watch = watch_config_from_toml(
        "[pr_watch]\nenabled = true\n\n[pr_watch.digest]\nenabled = true\n\
         command = [\"deliver\"]\ninterval_minutes = 30\n",
    );
    assert!(watch.digest);
    assert_eq!(watch.digest_command, vec!["deliver".to_owned()]);
    assert_eq!(watch.digest_policy.interval, Duration::minutes(30));
    assert!(watch.warnings.is_empty(), "{:?}", watch.warnings);
}

#[test]
fn a_bare_digest_boolean_without_a_table_is_still_honoured() {
    let on = watch_config_from_toml("[pr_watch]\ndigest = true\n");
    assert!(on.digest);
    assert!(on.warnings.is_empty());
    let off = watch_config_from_toml("[pr_watch]\ndigest = false\n");
    assert!(!off.digest);
    assert!(off.warnings.is_empty());
}

#[test]
fn a_digest_table_without_enabled_stays_off_and_warns() {
    let watch = watch_config_from_toml("[pr_watch.digest]\ncommand = [\"deliver\"]\n");
    assert!(!watch.digest);
    assert_eq!(watch.digest_command, vec!["deliver".to_owned()]);
    assert_eq!(watch.warnings.len(), 1);
    assert!(
        watch.warnings[0].contains("enabled = true"),
        "{:?}",
        watch.warnings
    );
    // A wrongly typed toggle is also off and loud, never silently false.
    let typo = watch_config_from_toml("[pr_watch.digest]\nenabled = \"yes\"\n");
    assert!(!typo.digest);
    assert_eq!(typo.warnings.len(), 1);
}

#[test]
fn the_documented_daemon_config_parses_and_enables_the_digest() {
    let doc = include_str!("../../docs/pr-watch.md");
    let start = doc
        .find("```toml\n[pr_watch]\n")
        .expect("docs/pr-watch.md has the daemon config example");
    let body = &doc[start + "```toml\n".len()..];
    let body = &body[..body.find("```").expect("closing fence")];
    let watch = watch_config_from_toml(body);
    assert!(
        !watch.enabled,
        "the example documents the job off by default"
    );
    assert!(watch.digest, "the example documents the digest enabled");
    assert!(!watch.digest_command.is_empty());
    assert_eq!(watch.digest_policy.interval, Duration::minutes(60));
    assert_eq!(watch.digest_policy.min_age, Duration::minutes(120));
    assert_eq!(watch.repos, vec!["Generous-Corp/pulp".to_owned()]);
    assert_eq!(watch.thresholds.red_minutes, 30);
    assert!(watch.warnings.is_empty(), "{:?}", watch.warnings);
}

#[test]
fn a_daemon_pass_carries_config_warnings() {
    let global = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    // No repositories, so the pass reads nothing but still reports config.
    std::fs::write(
        global.path().join("config.toml"),
        "[pr_watch]\nenabled = true\nrepos = []\n\n[pr_watch.digest]\ncommand = [\"x\"]\n",
    )
    .unwrap();
    let pass = super::scan::daemon_pass(global.path(), state.path(), &[], chrono::Utc::now());
    assert!(pass.enabled);
    assert_eq!(pass.warnings.len(), 1, "{:?}", pass.warnings);
}
