use std::cell::RefCell;

use chrono::{DateTime, Utc};

use super::*;
use crate::environment_requeue::{CheckAttribution, StepEvidence, StepReading};

const REPO: &str = "Generous-Corp/pulp";
const MIDI: &str = "CoreMIDI virtual port delivers SysEx";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-13T23:00:00Z")
        .expect("time")
        .with_timezone(&Utc)
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
    }
}

struct Case<'a> {
    policy: FlakePolicy,
    pr: u64,
    head: &'a str,
    at: &'a str,
    verdict: EnvironmentRequeue,
    defect_fix_removed: bool,
    base_red: Vec<String>,
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
            base_red: Vec::new(),
        }
    }

    fn run(&self, ledger: &mut StewardLedger) -> Option<Verdict> {
        consider(
            &Ejection {
                policy: &self.policy,
                repo: REPO,
                pr: self.pr,
                head: self.head,
                at: Some(self.at),
                verdict: &self.verdict,
                defect_fix_removed: self.defect_fix_removed,
                base_red: &self.base_red,
            },
            ledger,
            None,
            now(),
        )
    }
}

fn requeued(verdict: Option<&Verdict>) -> bool {
    matches!(verdict, Some(Verdict::Requeue(_)))
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
    assert!(requeued(verdict.as_ref()), "{verdict:?}");
    let Some(Verdict::Requeue(detail)) = verdict else {
        unreachable!()
    };
    assert!(
        detail.contains(&format!("flake:macos: {MIDI}")) && detail.contains("#12201"),
        "{detail}"
    );
    // The first one is now corroborated too, and gets its own one requeue.
    assert!(requeued(first.run(&mut ledger).as_ref()));
    // No second requeue at the same head, for either.
    for case in [&first, &second] {
        match case.run(&mut ledger) {
            Some(Verdict::Keep(detail)) => assert!(detail.contains("was spent"), "{detail}"),
            other => panic!("expected the spent refusal, got {other:?}"),
        }
    }
    assert_eq!(ledger.flake_requeue.requeued.len(), 2);
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
    // A new head of the same pull request is a new head: it may be
    // requeued once more if the flake ejects it again.
    let pushed = Case::new(12201, "cccc", "2026-11-13T22:00:00Z", "g3");
    assert!(requeued(pushed.run(&mut ledger).as_ref()));
}

#[test]
fn a_single_ejection_is_not_a_flake() {
    let mut ledger = StewardLedger::default();
    let case = Case::new(12201, "aaaa", "2026-11-13T20:00:00Z", "g1");
    assert_eq!(case.run(&mut ledger), None);
    // Seen again on a later pass: still one pull request.
    assert_eq!(case.run(&mut ledger), None);
    assert_eq!(ledger.flake_requeue.sightings.len(), 1);
    assert!(ledger.flake_requeue.requeued.is_empty());
    assert!(ledger.flake_requeue.pass.is_empty());
}

#[test]
fn one_group_ejecting_two_members_or_a_far_or_different_failure_is_not_a_flake() {
    // One merge group that ejected two of its members is one failure.
    let mut ledger = StewardLedger::default();
    assert_eq!(
        Case::new(1, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger),
        None
    );
    assert_eq!(
        Case::new(2, "bbbb", "2026-11-13T20:00:00Z", "g1").run(&mut ledger),
        None
    );
    // Outside the window.
    let mut ledger = StewardLedger::default();
    Case::new(1, "aaaa", "2026-11-11T20:00:00Z", "g1").run(&mut ledger);
    assert_eq!(
        Case::new(2, "bbbb", "2026-11-13T21:00:00Z", "g2").run(&mut ledger),
        None
    );
    // A different test.
    let mut ledger = StewardLedger::default();
    Case::new(1, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger);
    let mut other = Case::new(2, "bbbb", "2026-11-13T21:00:00Z", "g2");
    other.verdict = verdict("g2", "macos", &["Graphite shadows"]);
    assert_eq!(other.run(&mut ledger), None);
    // A failure that names no test has no signature at all.
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
    assert!(
        !ledger
            .flake_requeue
            .requeued
            .contains_key(&receipt_key(REPO, 12202, "bbbb"))
    );
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
    assert_eq!(ledger.flake_requeue.pass[0].outcome, "would_requeue");
    // Off records nothing at all.
    let mut ledger = StewardLedger::default();
    let mut off = Case::new(12201, "aaaa", "2026-11-13T20:00:00Z", "g1");
    off.policy.mode = FlakeMode::Off;
    assert_eq!(off.run(&mut ledger), None);
    assert!(ledger.flake_requeue.is_empty());
}

#[test]
fn a_base_red_on_the_check_or_an_allowed_retry_is_not_a_flake_requeue() {
    let mut ledger = StewardLedger::default();
    Case::new(12201, "aaaa", "2026-11-13T20:00:00Z", "g1").run(&mut ledger);
    let mut red = Case::new(12202, "bbbb", "2026-11-13T21:00:00Z", "g2");
    red.base_red = vec!["macos".to_owned()];
    assert!(
        matches!(red.run(&mut ledger), Some(Verdict::Keep(detail)) if detail.contains("confirmed red"))
    );
    // The classifier already allows a retry: nothing to add.
    let mut allowed = Case::new(12203, "dddd", "2026-11-13T21:30:00Z", "g3");
    allowed.verdict.allowed = true;
    assert_eq!(allowed.run(&mut ledger), None);
}

// ── root-cause issues ──────────────────────────────────────────────────────

/// A fake `gh` that records every call and answers issue writes.
struct FakeGh {
    calls: RefCell<Vec<String>>,
    open: String,
    refuse: bool,
}

impl FakeGh {
    fn new(open: &str) -> Self {
        Self {
            calls: RefCell::default(),
            open: open.to_owned(),
            refuse: false,
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
        } else if line.contains("/comments") {
            Ok("{}".to_owned())
        } else if line.contains("repos/Generous-Corp/pulp/issues") {
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

fn flake_ledger(prs: &[u64]) -> StewardLedger {
    let mut ledger = StewardLedger::default();
    for (index, pr) in prs.iter().enumerate() {
        Case::new(
            *pr,
            &format!("{pr:040x}"),
            &format!("2026-11-13T2{index}:00:00Z"),
            &format!("g{index}"),
        )
        .run(&mut ledger);
    }
    ledger
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
    let writes = gh.writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].contains(ISSUE_MARKER), "{}", writes[0]);
    // Only the second ejection was decided: the first had no company yet.
    assert_eq!(status.decisions.len(), 1, "{:?}", status.decisions);
    assert!(
        ledger.flake_requeue.pass.is_empty(),
        "decisions are drained"
    );

    // The next pass, nothing new: no write, no listing.
    let gh = FakeGh::new("");
    let status = finish_with(&gh, &mut ledger);
    assert_eq!(status.flake_root_cause[0].issue_action, "recorded");
    assert!(gh.calls.borrow().is_empty(), "{:?}", gh.calls.borrow());

    // A third pull request joins: one comment on the same issue.
    Case::new(
        12203,
        &format!("{:040x}", 12203),
        "2026-11-13T22:30:00Z",
        "g9",
    )
    .run(&mut ledger);
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
fn an_issue_the_ledger_lost_is_found_by_its_marker() {
    let mut ledger = flake_ledger(&[12201, 12202]);
    let key = issue_key(REPO, &format!("macos: {MIDI}"));
    let open = format!(
        "{}\n{}",
        serde_json::json!({"number": 77, "body": format!("{ISSUE_MARKER}{key} -->")}),
        serde_json::json!({"number": 78, "body": format!("{ISSUE_MARKER}other -->")}),
    );
    let gh = FakeGh::new(&open);
    let status = finish_with(&gh, &mut ledger);
    let cause = &status.flake_root_cause[0];
    assert_eq!((cause.issue, cause.issue_action), (Some(77), "found"));
    assert!(gh.writes().is_empty(), "{:?}", gh.writes());
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
    // The next pass reports it and writes nothing.
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
            window_hours: 24,
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
