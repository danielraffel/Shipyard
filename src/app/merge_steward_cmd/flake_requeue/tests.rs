use std::cell::RefCell;

use chrono::{DateTime, Utc};

use super::*;
use crate::base_health::fix_first::TipReading;
use crate::base_health::merge_group::{GroupRef, PoisonedContext};
use crate::environment_requeue::{CheckAttribution, StepEvidence, StepReading};

const REPO: &str = "Generous-Corp/pulp";
const MIDI: &str = "CoreMIDI virtual port delivers SysEx";

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("time")
        .with_timezone(&Utc)
}

fn now() -> DateTime<Utc> {
    at("2026-11-13T23:00:00Z")
}

/// A refused classifier verdict: `check` failed on merge group `group`,
/// its one failing step naming `tests`.
fn verdict(group: &str, check: &str, tests: &[&str]) -> EnvironmentRequeue {
    EnvironmentRequeue {
        allowed: false,
        reason: "not retried".to_owned(),
        class: None,
        merge_group_commit: Some(group.to_owned()),
        evidence: vec![StepEvidence {
            check: check.to_owned(),
            job_id: 7,
            step: "Test".to_owned(),
            reading: StepReading::NoSignature,
            infra: None,
            failed_tests: tests.iter().map(|test| (*test).to_owned()).collect(),
        }],
        attribution: vec![CheckAttribution {
            check: check.to_owned(),
            attribution: FailureAttribution::NoInfraSignature("no signature".to_owned()),
        }],
    }
}

fn apply() -> FlakePolicy {
    FlakePolicy {
        mode: FlakeMode::Apply,
        window_hours: 24,
        max_requeues: 3,
    }
}

struct Case<'a> {
    policy: FlakePolicy,
    pr: u64,
    head: &'a str,
    at: &'a str,
    verdict: EnvironmentRequeue,
    defect_fix_removed: bool,
    poison: Option<RepoPoison>,
    /// What the base tip answers for every check.
    tip: TipAnswer,
    now: DateTime<Utc>,
}

impl<'a> Case<'a> {
    fn new(pr: u64, head: &'a str, at: &'a str, group: &str) -> Self {
        Self {
            policy: apply(),
            pr,
            head,
            at,
            verdict: verdict(group, "macos", &[MIDI]),
            defect_fix_removed: false,
            poison: Some(green_base(&["macos"])),
            tip: Ok(Some("success".to_owned())),
            now: now(),
        }
    }

    fn run_at(&self, ledger: &mut StewardLedger, path: Option<&Path>) -> Option<Verdict> {
        let tip = |_: &str| self.tip.clone();
        consider(
            &Ejection {
                policy: &self.policy,
                repo: REPO,
                pr: self.pr,
                head: self.head,
                at: Some(self.at),
                verdict: &self.verdict,
                defect_fix_removed: self.defect_fix_removed,
                poison: self.poison.as_ref(),
                tip: &tip,
            },
            ledger,
            path,
            self.now,
        )
    }

    fn run(&self, ledger: &mut StewardLedger) -> Option<Verdict> {
        self.run_at(ledger, None)
    }
}

fn requeued(verdict: Option<&Verdict>) -> bool {
    matches!(verdict, Some(Verdict::Requeue(_)))
}

fn last_outcome(ledger: &StewardLedger) -> &'static str {
    ledger
        .flake_requeue
        .pass
        .last()
        .expect("a decision")
        .outcome
}

/// Mark a head's pending receipt done, as the enqueue path does once GitHub
/// accepts the enqueue.
fn accepted(ledger: &mut StewardLedger, pr: u64, head: &str) {
    settle(ledger, None, REPO, pr, head, Some("enqueued"));
}

#[test]
fn two_prs_ejected_on_the_same_signature_are_flake_and_requeue_once() {
    let mut ledger = StewardLedger::default();
    let first = Case::new(12201, "aaaa", "2026-11-13T20:00:00Z", "g1");
    // The first ejection alone is no flake.
    assert_eq!(first.run(&mut ledger), None);
    // A second pull request, a different merge group, the same test.
    let second = Case::new(12202, "bbbb", "2026-11-13T21:00:00Z", "g2");
    let verdict = second.run(&mut ledger);
    let Some(Verdict::Requeue(detail)) = verdict else {
        panic!("{verdict:?}")
    };
    assert!(
        detail.contains(&format!("flake:macos: {MIDI}")) && detail.contains("#12201"),
        "{detail}"
    );
    accepted(&mut ledger, 12202, "bbbb");
    // The first one is now corroborated too, and gets its own one requeue.
    assert!(requeued(first.run(&mut ledger).as_ref()));
    accepted(&mut ledger, 12201, "aaaa");
    // No second requeue at the same head, for either.
    for case in [&first, &second] {
        match case.run(&mut ledger) {
            Some(Verdict::Keep(detail)) => assert!(detail.contains("was spent"), "{detail}"),
            other => panic!("expected the spent refusal, got {other:?}"),
        }
    }
    let outcomes = ledger
        .flake_requeue
        .pass
        .iter()
        .map(|decision| (decision.pr, decision.outcome))
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        vec![
            (12202, "requeued"),
            (12201, "requeued"),
            (12201, "already_requeued"),
            (12202, "already_requeued"),
        ]
    );
    assert!(
        ledger
            .flake_requeue
            .requeued
            .values()
            .all(|receipt| receipt.state == ReceiptState::Done)
    );
}

#[test]
fn a_single_ejection_is_not_a_flake() {
    let mut ledger = StewardLedger::default();
    let case = Case::new(12201, "aaaa", "2026-11-13T20:00:00Z", "g1");
    assert_eq!(case.run(&mut ledger), None);
    assert_eq!(case.run(&mut ledger), None);
    assert_eq!(ledger.flake_requeue.sightings.len(), 1);
    assert!(ledger.flake_requeue.requeued.is_empty());
    assert!(ledger.flake_requeue.pass.is_empty());
}

#[test]
fn one_group_ejecting_two_members_or_a_far_or_different_failure_is_not_a_flake() {
    let mut ledger = StewardLedger::default();
    assert_eq!(
        Case::new(1, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger),
        None
    );
    assert_eq!(
        Case::new(2, "bbbb", "2026-11-13T20:00:00Z", "g1").run(&mut ledger),
        None
    );
    let mut ledger = StewardLedger::default();
    Case::new(1, "aaaa", "2026-11-11T20:00:00Z", "g1").run(&mut ledger);
    assert_eq!(
        Case::new(2, "bbbb", "2026-11-13T21:00:00Z", "g2").run(&mut ledger),
        None
    );
    let mut ledger = StewardLedger::default();
    Case::new(1, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger);
    let mut other = Case::new(2, "bbbb", "2026-11-13T21:00:00Z", "g2");
    other.verdict = verdict("g2", "macos", &["Graphite shadows"]);
    assert_eq!(other.run(&mut ledger), None);
    let mut ledger = StewardLedger::default();
    let mut bare = Case::new(1, "aaaa", "2026-11-13T20:00:00Z", "g1");
    bare.verdict = verdict("g1", "macos", &[]);
    assert_eq!(bare.run(&mut ledger), None);
    bare.pr = 2;
    bare.verdict = verdict("g2", "macos", &[]);
    assert_eq!(bare.run(&mut ledger), None);
    assert!(ledger.flake_requeue.sightings.is_empty());
}

#[test]
fn a_defect_fix_removed_head_is_never_requeued() {
    let mut ledger = StewardLedger::default();
    Case::new(12201, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger);
    let mut removed = Case::new(12202, "bbbb", "2026-11-13T21:00:00Z", "g2");
    removed.defect_fix_removed = true;
    for _ in 0..2 {
        match removed.run(&mut ledger) {
            Some(Verdict::Keep(detail)) => assert!(detail.contains("defect-fix"), "{detail}"),
            other => panic!("a defect-fix head must keep its refusal, got {other:?}"),
        }
    }
    assert!(ledger.flake_requeue.requeued.is_empty());
}

#[test]
fn shadow_reports_would_requeue_and_records_nothing_to_spend() {
    let mut ledger = StewardLedger::default();
    let mut first = Case::new(12201, "aaaa", "2026-11-13T20:00:00Z", "g1");
    first.policy.mode = FlakeMode::Shadow;
    first.run(&mut ledger);
    let mut second = Case::new(12202, "bbbb", "2026-11-13T21:00:00Z", "g2");
    second.policy.mode = FlakeMode::Shadow;
    match second.run(&mut ledger) {
        Some(Verdict::Keep(detail)) => assert!(detail.contains("would re-enqueue"), "{detail}"),
        other => panic!("{other:?}"),
    }
    assert!(ledger.flake_requeue.requeued.is_empty());
    assert_eq!(last_outcome(&ledger), "would_requeue");
    let mut ledger = StewardLedger::default();
    let mut off = Case::new(12201, "aaaa", "2026-11-13T20:00:00Z", "g1");
    off.policy.mode = FlakeMode::Off;
    assert_eq!(off.run(&mut ledger), None);
    assert!(ledger.flake_requeue.is_empty());
}

// ── the base reading fails closed ──────────────────────────────────────────

fn poisoned_macos(tip: TipReading) -> RepoPoison {
    RepoPoison {
        health: MergeGroupHealth::Poisoned {
            contexts: vec![PoisonedContext {
                context: "macos".to_owned(),
                first_error: None,
                groups: vec![GroupRef {
                    head_sha: "g1".to_owned(),
                    prs: vec![12201],
                }],
                latest_failure_at: None,
                check_id: None,
                same_first_error: None,
            }],
        },
        tip: BTreeMap::from([("macos".to_owned(), tip)]),
        ..green_base(&[])
    }
}

fn tip(red: bool, error: Option<&str>) -> TipReading {
    TipReading {
        sha: None,
        red,
        red_since: None,
        error: error.map(str::to_owned),
    }
}

/// What the base tip answers for a check.
type TipAnswer = Result<Option<String>, String>;

#[test]
fn only_a_base_tip_read_success_for_the_checks_allows_a_requeue() {
    let success = || Ok(Some("success".to_owned()));
    let cases: Vec<(Option<RepoPoison>, TipAnswer, &str)> = vec![
        // Base poison off, or not read this pass.
        (None, success(), "base_unknown"),
        // The merge groups could not be read.
        (
            Some(RepoPoison {
                health: MergeGroupHealth::Unknown {
                    detail: "HTTP 502".to_owned(),
                },
                ..green_base(&[])
            }),
            success(),
            "base_unknown",
        ),
        // A streak whose tip read failed: partial.
        (
            Some(poisoned_macos(tip(false, Some("HTTP 502")))),
            success(),
            "base_unknown",
        ),
        // A partial reading of the named fixes.
        (
            Some(RepoPoison {
                fix_read_error: Some("fixes unread".to_owned()),
                ..green_base(&["macos"])
            }),
            success(),
            "base_unknown",
        ),
        // Confirmed red on the check.
        (Some(poisoned_macos(tip(true, None))), success(), "base_red"),
        // The motivating case: a streak (the flake), the tip not red, and
        // its newest completed run a success.
        (
            Some(poisoned_macos(tip(false, None))),
            success(),
            "requeued",
        ),
        (Some(green_base(&["macos"])), success(), "requeued"),
        // The tip has no completed run (empty, pending, cancelled).
        (
            Some(poisoned_macos(tip(false, None))),
            Ok(None),
            "base_unknown",
        ),
        (Some(green_base(&["macos"])), Ok(None), "base_unknown"),
        // Neutral or skipped is not a success.
        (
            Some(green_base(&["macos"])),
            Ok(Some("neutral".to_owned())),
            "base_unknown",
        ),
        (
            Some(green_base(&["macos"])),
            Ok(Some("skipped".to_owned())),
            "base_unknown",
        ),
        // The tip failed it.
        (
            Some(green_base(&["macos"])),
            Ok(Some("failure".to_owned())),
            "base_red",
        ),
        // The tip could not be read.
        (
            Some(green_base(&["macos"])),
            Err("HTTP 502".to_owned()),
            "base_unknown",
        ),
    ];
    for (poison, answer, expected) in cases {
        let mut ledger = StewardLedger::default();
        Case::new(12201, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger);
        let mut case = Case::new(12202, "bbbb", "2026-11-13T21:00:00Z", "g2");
        case.poison = poison.clone();
        case.tip = answer.clone();
        let verdict = case.run(&mut ledger);
        assert_eq!(
            last_outcome(&ledger),
            expected,
            "{poison:?} / {answer:?}: {verdict:?}"
        );
        assert_eq!(requeued(verdict.as_ref()), expected == "requeued");
        assert_eq!(
            ledger.flake_requeue.requeued.is_empty(),
            expected != "requeued"
        );
    }
    // No checks: never green.
    assert!(matches!(
        base_read(Some(&green_base(&["macos"])), &[], &|_| success()),
        BaseRead::Unknown(_)
    ));
}

#[test]
fn the_tip_verdict_is_the_newest_completed_pass_or_fail_run() {
    let runs = |runs: serde_json::Value| serde_json::json!({ "check_runs": runs });
    let run = |id: u64, status: &str, conclusion: Option<&str>| serde_json::json!({"id": id, "name": "macos", "status": status, "conclusion": conclusion});
    // Empty.
    assert_eq!(
        newest_tip_verdict(&runs(serde_json::json!([])), "macos", None),
        None
    );
    // Only pending, or only cancelled: nothing says anything.
    assert_eq!(
        newest_tip_verdict(
            &runs(serde_json::json!([
                run(1, "in_progress", None),
                run(2, "queued", None)
            ])),
            "macos",
            None
        ),
        None
    );
    assert_eq!(
        newest_tip_verdict(
            &runs(serde_json::json!([run(1, "completed", Some("cancelled"))])),
            "macos",
            None
        ),
        None
    );
    // A newer pending run does not hide the newest completed one; a newer
    // cancel does not either.
    assert_eq!(
        newest_tip_verdict(
            &runs(serde_json::json!([
                run(1, "completed", Some("failure")),
                run(2, "completed", Some("success")),
                run(3, "in_progress", None),
                run(4, "completed", Some("cancelled")),
            ])),
            "macos",
            None
        )
        .as_deref(),
        Some("success")
    );
    // Another check's run, or another app's, says nothing.
    let other = serde_json::json!([
        {"id": 5, "name": "linux", "status": "completed", "conclusion": "success"},
        {"id": 6, "name": "macos", "status": "completed", "conclusion": "success", "app": {"id": 1}},
    ]);
    assert_eq!(newest_tip_verdict(&runs(other), "macos", Some(2)), None);
}

#[test]
fn an_allowed_classifier_retry_is_left_to_the_classifier() {
    let mut ledger = StewardLedger::default();
    Case::new(12201, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger);
    let mut allowed = Case::new(12203, "dddd", "2026-11-13T21:30:00Z", "g3");
    allowed.verdict.allowed = true;
    assert_eq!(allowed.run(&mut ledger), None);
}

// ── the breaker and the cap ────────────────────────────────────────────────

#[test]
fn a_requeued_head_ejected_again_trips_the_signature() {
    let mut ledger = StewardLedger::default();
    Case::new(1, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger);
    let second = Case::new(2, "bbbb", "2026-11-13T21:00:00Z", "g2");
    assert!(requeued(second.run(&mut ledger).as_ref()));
    accepted(&mut ledger, 2, "bbbb");
    // The re-enqueued head is ejected again, after the re-enqueue, the same
    // way: not a flake.
    let mut again = Case::new(2, "bbbb", "2026-11-13T23:20:00Z", "g4");
    again.now = at("2026-11-13T23:30:00Z");
    assert!(!requeued(again.run(&mut ledger).as_ref()));
    assert_eq!(last_outcome(&ledger), "signature_tripped");
    let trip = ledger
        .flake_requeue
        .tripped
        .values()
        .next()
        .expect("tripped");
    assert_eq!((trip.pr, trip.at.as_str()), (2, "2026-11-13T23:20:00Z"));
    // Every later pull request on the signature, and the first one, is
    // refused.
    for (pr, head) in [(1, "aaaa"), (3, "cccc")] {
        let mut later = Case::new(pr, head, "2026-11-13T23:25:00Z", "g5");
        later.now = at("2026-11-13T23:40:00Z");
        assert!(!requeued(later.run(&mut ledger).as_ref()));
        assert_eq!(last_outcome(&ledger), "signature_tripped");
    }
    // The root cause says so.
    let gh = FakeGh::new("");
    let status = finish(
        &|argv: &[String]| gh.run(argv),
        &FlakePolicy {
            mode: FlakeMode::Shadow,
            ..apply()
        },
        REPO,
        "main",
        &mut ledger,
        true,
        at("2026-11-13T23:40:00Z"),
    );
    let cause = &status.flake_root_cause[0];
    assert_eq!(cause.state, "signature_tripped");
    assert!(
        cause
            .tripped_by
            .as_deref()
            .is_some_and(|by| by.contains("#2")),
        "{cause:?}"
    );
}

#[test]
fn a_signature_the_sweep_could_not_check_requeues_nothing_this_pass() {
    let mut ledger = StewardLedger::default();
    Case::new(1, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger);
    ledger.flake_requeue.unswept.insert(
        issue_key(REPO, &format!("macos: {MIDI}")),
        "#9 timeline unread: HTTP 502".to_owned(),
    );
    let case = Case::new(2, "bbbb", "2026-11-13T21:00:00Z", "g2");
    assert!(!requeued(case.run(&mut ledger).as_ref()));
    assert_eq!(last_outcome(&ledger), "breaker_unread");
}

#[test]
fn a_signature_stops_after_its_requeue_cap_within_the_window() {
    let mut ledger = StewardLedger::default();
    let policy = FlakePolicy {
        max_requeues: 2,
        ..apply()
    };
    let heads = ["h1", "h2", "h3", "h4"];
    let times = (0..heads.len())
        .map(|index| format!("2026-11-13T2{index}:00:00Z"))
        .collect::<Vec<_>>();
    let mut outcomes = Vec::new();
    for (index, head) in heads.iter().enumerate() {
        let mut case = Case::new(
            u64::try_from(index).expect("index") + 1,
            head,
            &times[index],
            &format!("g{index}"),
        );
        case.policy = policy.clone();
        if case.run(&mut ledger).is_some() {
            outcomes.push((case.pr, last_outcome(&ledger)));
        }
    }
    assert_eq!(
        outcomes,
        vec![(2, "requeued"), (3, "requeued"), (4, "signature_capped"),]
    );
}

// ── receipt timing ─────────────────────────────────────────────────────────

#[test]
fn a_pending_receipt_waits_a_tick_then_retries_once() {
    let mut ledger = StewardLedger::default();
    Case::new(1, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger);
    let mut case = Case::new(2, "bbbb", "2026-11-13T21:00:00Z", "g2");
    assert!(requeued(case.run(&mut ledger).as_ref()));
    let key = receipt_key(REPO, 2, "bbbb");
    assert_eq!(
        ledger.flake_requeue.requeued[&key].state,
        ReceiptState::Pending
    );
    // The enqueue was deferred (no `enqueued`): the receipt stays pending.
    settle(
        &mut ledger,
        None,
        REPO,
        2,
        "bbbb",
        Some("generated_path_batch:#77"),
    );
    assert_eq!(
        ledger.flake_requeue.requeued[&key].state,
        ReceiptState::Pending
    );
    // Within the tick: still pending, nothing re-enqueued.
    case.now = at("2026-11-13T23:03:00Z");
    assert!(!requeued(case.run(&mut ledger).as_ref()));
    assert_eq!(last_outcome(&ledger), "requeue_pending");
    // After the tick: one retry.
    case.now = at("2026-11-13T23:10:00Z");
    assert!(requeued(case.run(&mut ledger).as_ref()));
    assert_eq!(last_outcome(&ledger), "requeue_retried");
    assert_eq!(ledger.flake_requeue.requeued[&key].attempts, 2);
    // Deferred again: never a third attempt.
    case.now = at("2026-11-13T23:30:00Z");
    assert!(!requeued(case.run(&mut ledger).as_ref()));
    assert_eq!(last_outcome(&ledger), "already_requeued");
}

#[test]
fn a_receipt_that_cannot_be_saved_does_not_requeue() {
    let temp = tempfile::tempdir().expect("temp");
    // A directory where the ledger file should be: the save fails.
    let path = temp.path().join("ledger.json");
    std::fs::create_dir(&path).expect("dir");
    let mut ledger = StewardLedger::default();
    Case::new(1, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger);
    let verdict =
        Case::new(2, "bbbb", "2026-11-13T21:00:00Z", "g2").run_at(&mut ledger, Some(&path));
    assert!(
        matches!(&verdict, Some(Verdict::Keep(detail)) if detail.contains("could not be saved")),
        "{verdict:?}"
    );
    assert_eq!(last_outcome(&ledger), "unrecorded");
    assert!(ledger.flake_requeue.requeued.is_empty());
}

// ── root-cause issues ──────────────────────────────────────────────────────

/// A fake `gh` that records every call and answers issue reads and writes.
struct FakeGh {
    calls: RefCell<Vec<String>>,
    open: String,
    refuse: bool,
    closed: bool,
}

impl FakeGh {
    fn new(open: &str) -> Self {
        Self {
            calls: RefCell::default(),
            open: open.to_owned(),
            refuse: false,
            closed: false,
        }
    }

    fn run(&self, argv: &[String]) -> Result<String, String> {
        let line = argv.join(" ");
        self.calls.borrow_mut().push(line.clone());
        if self.refuse && line.contains("--method POST") {
            return Err("ghapp: refused: issue writes are not in the grammar".to_owned());
        }
        if line.contains("issues?state=open") {
            Ok(self.open.clone())
        } else if line == "api repos/Generous-Corp/pulp/issues/900" {
            Ok(format!(
                r#"{{"number":900,"state":"{}"}}"#,
                if self.closed { "closed" } else { "open" }
            ))
        } else if line.contains("/comments") {
            Ok("{}".to_owned())
        } else if line.contains("--method POST") && line.contains("repos/Generous-Corp/pulp/issues")
        {
            Ok(r#"{"number":900}"#.to_owned())
        } else {
            Err(format!("unexpected: {line}"))
        }
    }

    fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|call| call.contains("--method POST"))
            .cloned()
            .collect()
    }
}

fn flake_ledger_with(prs: &[u64], tests: &[&str]) -> StewardLedger {
    let mut ledger = StewardLedger::default();
    let heads = prs
        .iter()
        .map(|pr| format!("{pr:040x}"))
        .collect::<Vec<_>>();
    let times = (0..prs.len())
        .map(|index| format!("2026-11-13T2{index}:00:00Z"))
        .collect::<Vec<_>>();
    for (index, pr) in prs.iter().enumerate() {
        let mut case = Case::new(*pr, &heads[index], &times[index], &format!("g{index}"));
        case.verdict = verdict(&format!("g{index}"), "macos", tests);
        case.policy.mode = FlakeMode::Shadow;
        case.run(&mut ledger);
    }
    ledger
}

fn flake_ledger(prs: &[u64]) -> StewardLedger {
    flake_ledger_with(prs, &[MIDI])
}

fn finish_with(gh: &FakeGh, ledger: &mut StewardLedger) -> RepoStatus {
    finish(
        &|argv: &[String]| gh.run(argv),
        &apply(),
        REPO,
        "main",
        ledger,
        true,
        now(),
    )
}

#[test]
fn one_issue_per_signature_then_updated_never_duplicated() {
    let mut ledger = flake_ledger(&[12201, 12202]);
    let gh = FakeGh::new("");
    let status = finish_with(&gh, &mut ledger);
    let cause = &status.flake_root_cause[0];
    assert_eq!(
        (cause.issue, cause.issue_action),
        (Some(900), "opened"),
        "{cause:?}"
    );
    assert_eq!(cause.prs, vec![12201, 12202]);
    assert_eq!(cause.tests, vec![MIDI.to_owned()]);
    assert_eq!(cause.state, "active");
    let writes = gh.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].contains(ISSUE_MARKER), "{}", writes[0]);
    assert_eq!(status.decisions.len(), 1, "{:?}", status.decisions);
    assert!(
        ledger.flake_requeue.pass.is_empty(),
        "decisions are drained"
    );

    // The next pass, nothing new: no read, no write.
    let gh = FakeGh::new("");
    let status = finish_with(&gh, &mut ledger);
    assert_eq!(status.flake_root_cause[0].issue_action, "recorded");
    assert!(gh.calls.borrow().is_empty(), "{:?}", gh.calls.borrow());

    // A third pull request joins: one comment on the same, open issue.
    let mut third = Case::new(
        12203,
        "0000000000000000000000000000000000002fdb",
        "2026-11-13T22:30:00Z",
        "g9",
    );
    third.policy.mode = FlakeMode::Shadow;
    third.run(&mut ledger);
    let gh = FakeGh::new("");
    let status = finish_with(&gh, &mut ledger);
    let cause = &status.flake_root_cause[0];
    assert_eq!((cause.issue, cause.issue_action), (Some(900), "updated"));
    let writes = gh.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(
        writes[0].contains("issues/900/comments") && writes[0].contains("#12203"),
        "{}",
        writes[0]
    );
    assert!(!writes[0].contains("#12201"), "{}", writes[0]);
}

#[test]
fn a_closed_issue_is_recorded_and_left_alone() {
    let mut ledger = flake_ledger(&[12201, 12202]);
    finish_with(&FakeGh::new(""), &mut ledger);
    let mut third = Case::new(
        12203,
        "0000000000000000000000000000000000002fdb",
        "2026-11-13T22:30:00Z",
        "g9",
    );
    third.policy.mode = FlakeMode::Shadow;
    third.run(&mut ledger);
    let mut gh = FakeGh::new("");
    gh.closed = true;
    let status = finish_with(&gh, &mut ledger);
    assert_eq!(status.flake_root_cause[0].issue_action, "closed");
    assert!(gh.writes().is_empty(), "{:?}", gh.writes());
    // Later passes do not even read it.
    let gh = FakeGh::new("");
    let status = finish_with(&gh, &mut ledger);
    assert_eq!(status.flake_root_cause[0].issue_action, "closed");
    assert!(gh.calls.borrow().is_empty());
}

#[test]
fn a_tripped_breaker_is_added_to_the_issue() {
    let mut ledger = flake_ledger(&[12201, 12202]);
    finish_with(&FakeGh::new(""), &mut ledger);
    let key = issue_key(REPO, &format!("macos: {MIDI}"));
    ledger.flake_requeue.tripped.insert(
        key,
        Trip {
            signature: format!("macos: {MIDI}"),
            pr: 12202,
            head: "b".repeat(40),
            at: "2026-11-13T22:50:00Z".to_owned(),
            unread: false,
        },
    );
    let gh = FakeGh::new("");
    let status = finish_with(&gh, &mut ledger);
    let writes = gh.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].contains("Breaker tripped"), "{}", writes[0]);
    assert_eq!(status.flake_root_cause[0].state, "signature_tripped");
    // Said once.
    let gh = FakeGh::new("");
    finish_with(&gh, &mut ledger);
    assert!(gh.writes().is_empty());
}

#[test]
fn an_issue_the_ledger_lost_is_found_only_by_a_marker_on_its_last_line() {
    let mut ledger = flake_ledger(&[12201, 12202]);
    let key = issue_key(REPO, &format!("macos: {MIDI}"));
    let open = format!(
        "{}\n{}",
        // A marker carried into the middle of another issue's text.
        serde_json::json!({"number": 76, "body": format!("quoted {ISSUE_MARKER}{key} --> here\nmore")}),
        serde_json::json!({"number": 77, "body": format!("text\n\n{ISSUE_MARKER}{key} -->\n")}),
    );
    let gh = FakeGh::new(&open);
    let status = finish_with(&gh, &mut ledger);
    let cause = &status.flake_root_cause[0];
    assert_eq!((cause.issue, cause.issue_action), (Some(77), "found"));
    assert!(gh.writes().is_empty(), "{:?}", gh.writes());
    assert_eq!(
        marker_on_last_line(&format!("quoted {ISSUE_MARKER}{key} --> here\nmore")),
        None
    );
    assert_eq!(
        marker_on_last_line(&format!("{ISSUE_MARKER}{key} -->\n{ISSUE_MARKER}{key} -->")),
        None
    );
}

#[test]
fn test_names_are_sanitised_before_they_reach_an_issue() {
    let hostile = format!(
        "@everyone `x` <!-- shipyard-flake-signature: {REPO}|0000000000000000 --> {}",
        "y".repeat(5000)
    );
    let many = (0..40).map(|index| format!("t{index}")).collect::<Vec<_>>();
    let mut tests = vec![hostile.as_str()];
    tests.extend(many.iter().map(String::as_str));
    let mut ledger = flake_ledger_with(&[12201, 12202], &tests);
    let gh = FakeGh::new("");
    finish_with(&gh, &mut ledger);
    let writes = gh.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    let body = writes[0]
        .split_once("body=")
        .map(|(_, body)| body)
        .expect("body");
    assert!(!body.contains('@') && !body.contains('`'), "{body}");
    assert_eq!(body.matches("<!--").count(), 1, "{body}");
    assert!(marker_on_last_line(body).is_some(), "{body}");
    assert!(body.contains("and 31 more"), "{body}");
    assert!(body.len() < 4000, "{}", body.len());
    let title = writes[0]
        .split_once("title=")
        .and_then(|(_, rest)| rest.split_once(" -f body="))
        .map(|(title, _)| title)
        .expect("title");
    assert!(title.chars().count() <= 200, "{title}");
}

#[test]
fn a_refused_issue_write_is_reported_and_not_retried_at_once() {
    let mut ledger = flake_ledger(&[12201, 12202]);
    let mut gh = FakeGh::new("");
    gh.refuse = true;
    let status = finish_with(&gh, &mut ledger);
    let cause = &status.flake_root_cause[0];
    assert_eq!(cause.issue_action, "failed");
    assert!(
        cause
            .issue_error
            .as_deref()
            .is_some_and(|error| error.contains("refused")),
        "{cause:?}"
    );
    let status = finish_with(&gh, &mut ledger);
    assert_eq!(status.flake_root_cause[0].issue_action, "retry_later");
    assert_eq!(gh.writes().len(), 1);
}

#[test]
fn shadow_reports_the_root_cause_without_filing() {
    let mut ledger = flake_ledger(&[12201, 12202]);
    let gh = FakeGh::new("");
    let status = finish(
        &|argv: &[String]| gh.run(argv),
        &FlakePolicy {
            mode: FlakeMode::Shadow,
            ..apply()
        },
        REPO,
        "main",
        &mut ledger,
        true,
        now(),
    );
    let cause = &status.flake_root_cause[0];
    assert_eq!((cause.issue, cause.issue_action), (None, "would_file"));
    assert!(gh.calls.borrow().is_empty());
    let json = serde_json::to_value(&status).expect("json");
    assert_eq!(
        json["flake_root_cause"][0]["prs"],
        serde_json::json!([12201, 12202])
    );
}
