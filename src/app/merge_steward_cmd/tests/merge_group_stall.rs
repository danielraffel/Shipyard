//! Merge groups that can never finish, on the 2026-11-13 Vellum #406 shape:
//! group `df1ddd39`, "Framework trusted verify" failed at 13:50:16Z in its
//! `framework-trusted-judge` job, the required `framework-trusted-verify`
//! status (GitHub Actions app 15368) was never posted, every other run had
//! completed by 14:54:21Z.
use std::cell::RefCell;
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::*;
use crate::app::merge_steward_cmd::merge_group_stall::{
    self, Acts, Entry, GroupFacts, GroupRun, StallPass, Verdict,
};
use crate::base_health::PoisonHold;
use crate::merge_steward::RequiredCheck;

const GROUP: &str = "df1ddd39202e6544885d2d596d4f2b1159c753cf";
const REPO: &str = "Generous-Corp/vellum";
const ACTIONS: u64 = 15_368;

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("time")
        .with_timezone(&Utc)
}

fn required() -> Vec<RequiredCheck> {
    [
        "release-policy",
        "release-path-review",
        "framework-trusted-verify",
    ]
    .iter()
    .map(|context| RequiredCheck {
        context: (*context).to_owned(),
        app_id: Some(ACTIONS),
    })
    .collect()
}

fn run(
    id: u64,
    name: &str,
    path: &str,
    status: &str,
    conclusion: Option<&str>,
    done: &str,
) -> GroupRun {
    GroupRun {
        id,
        name: name.to_owned(),
        path: format!(".github/workflows/{path}"),
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        updated_at: Some(at(done)),
        url: Some(format!("https://github.com/{REPO}/actions/runs/{id}")),
    }
}

/// The real group facts.
fn vellum_406() -> GroupFacts {
    GroupFacts {
        runs: vec![
            run(
                43_437_128_575,
                "Release policy",
                "release-policy.yml",
                "completed",
                Some("success"),
                "2026-11-13T13:51:39Z",
            ),
            run(
                43_437_128_609,
                "Framework trusted verify",
                "framework-trusted-verify.yml",
                "completed",
                Some("failure"),
                "2026-11-13T13:50:16Z",
            ),
            run(
                43_437_128_696,
                "Locked macOS GPU proof",
                "gpu-macos.yml",
                "completed",
                Some("success"),
                "2026-11-13T14:54:21Z",
            ),
        ],
        checks: vec![
            ("release-policy".to_owned(), Some(ACTIONS)),
            ("framework-trusted-judge".to_owned(), Some(ACTIONS)),
        ],
        statuses: vec![("release-path-review".to_owned(), Some(ACTIONS))],
    }
}

#[test]
fn the_vellum_406_group_is_doomed_as_soon_as_every_run_is_done() {
    let verdict = merge_group_stall::judge(
        &vellum_406(),
        &required(),
        &BTreeMap::new(),
        None,
        at("2026-11-13T14:55:00Z"),
    );
    let Verdict::Stalled {
        missing, doomed, ..
    } = verdict
    else {
        panic!("{verdict:?}");
    };
    assert_eq!(missing, vec!["framework-trusted-verify"]);
    let doomed = doomed.expect("its producer failed");
    assert_eq!(doomed.run_id, 43_437_128_609);
    assert_eq!(doomed.conclusion, "failure");
}

#[test]
fn a_run_still_in_progress_decides_nothing() {
    let mut facts = vellum_406();
    facts.runs[2].status = "in_progress".to_owned();
    assert_eq!(
        merge_group_stall::judge(
            &facts,
            &required(),
            &BTreeMap::new(),
            None,
            at("2026-11-13T18:00:00Z")
        ),
        Verdict::Running
    );
}

#[test]
fn an_absent_context_with_no_failed_producer_reports_only_after_thirty_minutes() {
    let mut facts = vellum_406();
    facts.runs[1].conclusion = Some("success".to_owned());
    let watching = merge_group_stall::judge(
        &facts,
        &required(),
        &BTreeMap::new(),
        None,
        at("2026-11-13T15:20:00Z"),
    );
    assert!(matches!(watching, Verdict::Watching { .. }), "{watching:?}");
    let stalled = merge_group_stall::judge(
        &facts,
        &required(),
        &BTreeMap::new(),
        None,
        at("2026-11-13T15:25:00Z"),
    );
    assert!(
        matches!(stalled, Verdict::Stalled { doomed: None, .. }),
        "{stalled:?}"
    );
}

#[test]
fn a_context_from_another_app_is_not_present_and_a_producer_can_be_named_by_policy() {
    let mut facts = vellum_406();
    facts
        .statuses
        .push(("framework-trusted-verify".to_owned(), None));
    let verdict = merge_group_stall::judge(
        &facts,
        &required(),
        &BTreeMap::new(),
        None,
        at("2026-11-13T14:55:00Z"),
    );
    assert!(
        matches!(verdict, Verdict::Stalled { .. }),
        "a status from no known app does not count"
    );
    // A producer whose names do not match is found through the policy.
    let mut renamed = vellum_406();
    renamed.runs[1].name = "Judge".to_owned();
    renamed.runs[1].path = ".github/workflows/judge.yml".to_owned();
    let unnamed = merge_group_stall::judge(
        &renamed,
        &required(),
        &BTreeMap::new(),
        None,
        at("2026-11-13T14:55:00Z"),
    );
    assert!(matches!(unnamed, Verdict::Watching { .. }), "{unnamed:?}");
    let producers = BTreeMap::from([(
        "framework-trusted-verify".to_owned(),
        "judge.yml".to_owned(),
    )]);
    let named = merge_group_stall::judge(
        &renamed,
        &required(),
        &producers,
        None,
        at("2026-11-13T14:55:00Z"),
    );
    assert!(
        matches!(
            named,
            Verdict::Stalled {
                doomed: Some(_),
                ..
            }
        ),
        "{named:?}"
    );
}

#[test]
fn a_partial_read_is_unread_and_decides_nothing() {
    let more_pages = |argv: &[String]| -> Result<String, String> {
        let line = argv.join(" ");
        if line.contains("actions/runs?head_sha=") {
            return Ok(r#"{"total_count":150,"workflow_runs":[]}"#.to_owned());
        }
        Err(format!("unexpected {line}"))
    };
    assert!(merge_group_stall::read_group(&more_pages, REPO, GROUP).is_err());
    let failing_statuses = |argv: &[String]| -> Result<String, String> {
        let line = argv.join(" ");
        if line.contains("actions/runs?head_sha=") {
            return Ok(r#"{"total_count":0,"workflow_runs":[]}"#.to_owned());
        }
        if line.contains("/check-runs") {
            return Ok(r#"{"total_count":0,"check_runs":[]}"#.to_owned());
        }
        Err("HTTP 502".to_owned())
    };
    let error = merge_group_stall::read_group(&failing_statuses, REPO, GROUP).expect_err("partial");
    assert!(error.contains("502"), "{error}");
}

fn queued_pr() -> ObservedPr {
    let mut pr = ready_pr();
    pr.fact.number = 406;
    pr.fact.queue_position = Some(1);
    pr
}

fn doomed_pass(mode: PoisonHold) -> StallPass {
    let pr = queued_pr();
    let verdict = merge_group_stall::judge(
        &vellum_406(),
        &required(),
        &BTreeMap::new(),
        None,
        at("2026-11-13T14:55:00Z"),
    );
    StallPass {
        mode,
        entries: BTreeMap::from([(
            406,
            Entry {
                pr: 406,
                pr_head: pr.fact.head_sha.clone(),
                group_sha: GROUP.to_owned(),
                verdict,
            },
        )]),
        unread: BTreeMap::new(),
        defect_fix: crate::app::merge_steward_cmd::defect_fix_removals::Reading::default(),
    }
}

struct FakeActs {
    reread: Result<(String, Option<String>), String>,
    dequeue: Result<Option<String>, String>,
    comment: Result<(), String>,
    dequeued: RefCell<u32>,
    comments: RefCell<Vec<String>>,
}

impl FakeActs {
    fn ok(head: &str) -> Self {
        Self {
            reread: Ok((head.to_owned(), Some(GROUP.to_owned()))),
            dequeue: Ok(None),
            comment: Ok(()),
            dequeued: RefCell::new(0),
            comments: RefCell::default(),
        }
    }
}

impl Acts for FakeActs {
    fn reread(&self, _pr: &ObservedPr) -> Result<(String, Option<String>), String> {
        self.reread.clone()
    }
    fn dequeue(
        &self,
        _pr: &ObservedPr,
        _ledger: &mut StewardLedger,
    ) -> Result<Option<String>, String> {
        *self.dequeued.borrow_mut() += 1;
        self.dequeue.clone()
    }
    fn comment(&self, _pr: u64, body: &str) -> Result<(), String> {
        self.comments.borrow_mut().push(body.to_owned());
        self.comment.clone()
    }
}

fn queued() -> StewardDecision {
    StewardDecision::Queued { position: 1 }
}

#[test]
fn under_on_the_doomed_entry_is_dequeued_once_with_one_comment() {
    let pr = queued_pr();
    let acts = FakeActs::ok(&pr.fact.head_sha);
    let mut ledger = StewardLedger::default();
    let pass = doomed_pass(PoisonHold::On);
    let report = merge_group_stall::decide(
        &pass,
        true,
        REPO,
        &pr,
        &queued(),
        &mut ledger,
        &acts,
        Utc::now(),
    )
    .expect("acts");
    assert_eq!(
        report.mutation.as_deref(),
        Some("doomed_group_dequeued:framework-trusted-verify:df1ddd39202e")
    );
    assert_eq!(report.error, None);
    assert_eq!(*acts.dequeued.borrow(), 1);
    let comments = acts.comments.borrow();
    assert_eq!(comments.len(), 1);
    assert!(
        comments[0].contains("`framework-trusted-verify`"),
        "{}",
        comments[0]
    );
    assert!(
        comments[0].contains("actions/runs/43437128609"),
        "{}",
        comments[0]
    );
    assert!(
        comments[0].contains("concluded `failure`"),
        "{}",
        comments[0]
    );
    drop(comments);
    // Once per (PR, group head).
    let again = merge_group_stall::decide(
        &pass,
        true,
        REPO,
        &pr,
        &queued(),
        &mut ledger,
        &acts,
        Utc::now(),
    )
    .expect("reports");
    assert!(
        again
            .mutation
            .as_deref()
            .is_some_and(|m| m.starts_with("doomed_group_already_decided"))
    );
    assert_eq!(*acts.dequeued.borrow(), 1);
}

#[test]
fn under_dry_run_the_doomed_entry_is_reported_and_nothing_changes() {
    let pr = queued_pr();
    let acts = FakeActs::ok(&pr.fact.head_sha);
    let mut ledger = StewardLedger::default();
    let report = merge_group_stall::decide(
        &doomed_pass(PoisonHold::DryRun),
        true,
        REPO,
        &pr,
        &queued(),
        &mut ledger,
        &acts,
        Utc::now(),
    )
    .expect("reports");
    assert_eq!(
        report.mutation.as_deref(),
        Some("would_dequeue_doomed_group:framework-trusted-verify:df1ddd39202e")
    );
    assert_eq!(*acts.dequeued.borrow(), 0);
    assert!(acts.comments.borrow().is_empty());
    assert!(ledger.doomed_dequeues.is_empty());
    assert!(
        merge_group_stall::decide(
            &doomed_pass(PoisonHold::Off),
            true,
            REPO,
            &pr,
            &queued(),
            &mut ledger,
            &acts,
            Utc::now()
        )
        .is_none()
    );
}

#[test]
fn a_moved_head_or_group_or_a_failed_reread_never_dequeues_and_a_failure_retries() {
    let pr = queued_pr();
    let pass = doomed_pass(PoisonHold::On);
    let mut moved = FakeActs::ok(&"b".repeat(40));
    let mut ledger = StewardLedger::default();
    let report = merge_group_stall::decide(
        &pass,
        true,
        REPO,
        &pr,
        &queued(),
        &mut ledger,
        &moved,
        Utc::now(),
    )
    .expect("r");
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("refused: head moved")),
        "{report:?}"
    );
    assert_eq!(*moved.dequeued.borrow(), 0);
    let mut ledger = StewardLedger::default();
    moved.reread = Ok((pr.fact.head_sha.clone(), Some("c".repeat(40))));
    let report = merge_group_stall::decide(
        &pass,
        true,
        REPO,
        &pr,
        &queued(),
        &mut ledger,
        &moved,
        Utc::now(),
    )
    .expect("r");
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("refused: merge group is now")),
        "{report:?}"
    );
    assert_eq!(*moved.dequeued.borrow(), 0);
    // A failed re-read is recorded as failed and the next pass tries again.
    let mut ledger = StewardLedger::default();
    let mut failing = FakeActs::ok(&pr.fact.head_sha);
    failing.reread = Err("HTTP 502".to_owned());
    merge_group_stall::decide(
        &pass,
        true,
        REPO,
        &pr,
        &queued(),
        &mut ledger,
        &failing,
        Utc::now(),
    )
    .expect("r");
    assert_eq!(*failing.dequeued.borrow(), 0);
    let ok = FakeActs::ok(&pr.fact.head_sha);
    let report = merge_group_stall::decide(
        &pass,
        true,
        REPO,
        &pr,
        &queued(),
        &mut ledger,
        &ok,
        Utc::now(),
    )
    .expect("r");
    assert!(
        report
            .mutation
            .as_deref()
            .is_some_and(|m| m.starts_with("doomed_group_dequeued")),
        "{report:?}"
    );
}

#[test]
fn a_failed_comment_is_reported_and_retried_once_posted() {
    let pr = queued_pr();
    let mut acts = FakeActs::ok(&pr.fact.head_sha);
    acts.comment = Err("HTTP 500".to_owned());
    let mut ledger = StewardLedger::default();
    let report = merge_group_stall::decide(
        &doomed_pass(PoisonHold::On),
        true,
        REPO,
        &pr,
        &queued(),
        &mut ledger,
        &acts,
        Utc::now(),
    )
    .expect("r");
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|e| e.contains("comment not posted")),
        "{report:?}"
    );
    let posted = RefCell::new(Vec::new());
    let lines = merge_group_stall::retry_comments(&mut ledger, REPO, &|pr, body| {
        posted.borrow_mut().push((pr, body.to_owned()));
        Ok(())
    });
    let posted = posted.into_inner();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].0, 406);
    // The retry posts the same detail as the first attempt: the context,
    // the failed run and its conclusion.
    assert_eq!(
        posted[0].1,
        acts.comments.borrow()[0],
        "the exact first comment"
    );
    assert!(
        posted[0].1.contains("`framework-trusted-verify`"),
        "{}",
        posted[0].1
    );
    assert!(
        posted[0].1.contains("actions/runs/43437128609"),
        "{}",
        posted[0].1
    );
    assert!(
        posted[0].1.contains("concluded `failure`"),
        "{}",
        posted[0].1
    );
    assert!(lines[0].contains("comment posted on retry"), "{lines:?}");
    assert!(
        merge_group_stall::retry_comments(&mut ledger, REPO, &|_, _| panic!("once")).is_empty()
    );
}

#[test]
fn a_stalled_row_reads_merge_group_stalled_and_reports_a_line() {
    let pass = doomed_pass(PoisonHold::DryRun);
    let mut reports = vec![PrReport {
        number: 406,
        head_sha: queued_pr().fact.head_sha,
        decision: queued(),
        mutation: None,
        error: None,
        blocked: crate::app::merge_steward_cmd::blocked_on::Stall {
            blocked_on: "queued".to_owned(),
            ..Default::default()
        },
    }];
    merge_group_stall::annotate(&pass, &mut reports);
    assert_eq!(
        reports[0].blocked.blocked_on,
        "merge_group_stalled:framework-trusted-verify"
    );
    assert!(
        reports[0]
            .blocked
            .blocked_detail
            .as_deref()
            .is_some_and(|detail| detail.starts_with("doomed: run 43437128609"))
    );
    let lines = pass.lines();
    assert!(
        lines[0].starts_with(
            "merge-group-stalled #406 group df1ddd39202e missing framework-trusted-verify"
        ),
        "{lines:?}"
    );
}

/// The pass reads only queue entries whose group head has no active run.
#[test]
fn an_entry_with_an_active_run_is_not_read() {
    let mut observation = observation_for(queued_pr(), true);
    observation.merge_group_heads = BTreeMap::from([(406, GROUP.to_owned())]);
    observation.runs = vec![crate::merge_steward::StewardRun {
        id: 1,
        workflow_id: 1,
        run_attempt: 1,
        workflow: "Locked macOS GPU proof".to_owned(),
        head_sha: GROUP.to_owned(),
        head_branch: "gh-readonly-queue/main/pr-406-x".to_owned(),
        status: "in_progress".to_owned(),
        event: "merge_group".to_owned(),
        pull_request_number: None,
        created_at: "2026-11-13T13:49:00Z".to_owned(),
        jobs: Vec::new(),
    }];
    let pass = merge_group_stall::observe(
        &|argv: &[String]| Err(format!("no read expected: {}", argv.join(" "))),
        &observation,
        PoisonHold::On,
        &BTreeMap::new(),
        Utc::now(),
    );
    assert!(
        pass.entries.is_empty() && pass.unread.is_empty(),
        "{pass:?}"
    );
}

/// `[merge_queue] dequeue_doomed_groups` defaults to dry-run on a merge-queue
/// base, `off` without one, and a typo never turns it on.
#[test]
fn the_mode_defaults_to_dry_run_and_a_typo_never_turns_it_on() {
    use crate::merge_steward::EjectionGuards;
    let none: toml::Table = toml::from_str("").expect("toml");
    assert_eq!(
        EjectionGuards::from_table(&none, true).dequeue_doomed_groups,
        PoisonHold::DryRun
    );
    assert_eq!(
        EjectionGuards::from_table(&none, false).dequeue_doomed_groups,
        PoisonHold::Off
    );
    let on: toml::Table =
        toml::from_str("[merge_queue]\ndequeue_doomed_groups = \"on\"\n").expect("toml");
    assert_eq!(
        EjectionGuards::from_table(&on, true).dequeue_doomed_groups,
        PoisonHold::On
    );
    let typo: toml::Table =
        toml::from_str("[merge_queue]\ndequeue_doomed_groups = \"onn\"\n").expect("toml");
    let guards = EjectionGuards::from_table(&typo, true);
    assert_eq!(guards.dequeue_doomed_groups, PoisonHold::DryRun);
    assert!(
        guards
            .dequeue_doomed_groups_error
            .as_deref()
            .is_some_and(|error| error.contains("merge_queue.dequeue_doomed_groups")),
        "{:?}",
        guards.dequeue_doomed_groups_error
    );
    assert_eq!(
        guards.read_error, None,
        "a typo here never degrades the other guards"
    );
}

/// A receipt with no recorded comment text is never retried with a generic
/// comment that would omit the context and the run.
#[test]
fn a_receipt_without_its_comment_text_is_not_retried_generically() {
    let mut ledger = StewardLedger::default();
    ledger.doomed_dequeues.insert(
        merge_group_stall::receipt_key(REPO, 406, GROUP),
        merge_group_stall::Receipt {
            at: "2026-11-13T17:05:03Z".to_owned(),
            outcome: "dequeued".to_owned(),
            commented: false,
            comment: None,
        },
    );
    let lines = merge_group_stall::retry_comments(&mut ledger, REPO, &|_, _| {
        panic!("no generic comment is posted")
    });
    assert!(lines[0].contains("not recorded; not retried"), "{lines:?}");
}

/// A completed run on the group head in the census does not suppress the
/// reading: the doomed group is still found.
#[test]
fn a_completed_run_in_the_census_does_not_suppress_the_reading() {
    let mut observation = observation_for(queued_pr(), true);
    observation.repo = REPO.to_owned();
    observation.required_checks = required();
    observation.merge_group_heads = BTreeMap::from([(406, GROUP.to_owned())]);
    observation.runs = vec![crate::merge_steward::StewardRun {
        id: 43_437_128_609,
        workflow_id: 1,
        run_attempt: 1,
        workflow: "Framework trusted verify".to_owned(),
        head_sha: GROUP.to_owned(),
        head_branch: "gh-readonly-queue/main/pr-406-x".to_owned(),
        status: "completed".to_owned(),
        event: "merge_group".to_owned(),
        pull_request_number: None,
        created_at: "2026-11-13T13:49:00Z".to_owned(),
        jobs: Vec::new(),
    }];
    let reads = RefCell::new(0);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        *reads.borrow_mut() += 1;
        let line = argv.join(" ");
        if line.contains("actions/runs?head_sha=") {
            return Ok(serde_json::json!({"total_count": 2, "workflow_runs": [
                {"id": 43_437_128_575_u64, "name": "Release policy", "path": ".github/workflows/release-policy.yml",
                 "status": "completed", "conclusion": "success", "updated_at": "2026-11-13T13:51:39Z", "head_sha": GROUP},
                {"id": 43_437_128_609_u64, "name": "Framework trusted verify", "path": ".github/workflows/framework-trusted-verify.yml",
                 "status": "completed", "conclusion": "failure", "updated_at": "2026-11-13T13:50:16Z", "head_sha": GROUP,
                 "html_url": "https://github.com/Generous-Corp/vellum/actions/runs/43437128609"},
            ]}).to_string());
        }
        if line.contains("/check-runs") {
            return Ok(serde_json::json!({"total_count": 1, "check_runs": [
                {"name": "release-policy", "app": {"id": ACTIONS}},
            ]})
            .to_string());
        }
        if line.contains("/statuses") {
            return Ok(serde_json::json!([
                {"context": "release-path-review", "creator": {"login": "github-actions[bot]", "id": 41_898_282}},
            ]).to_string());
        }
        Err(format!("unexpected {line}"))
    };
    let pass = merge_group_stall::observe(
        &run_gh,
        &observation,
        PoisonHold::On,
        &BTreeMap::new(),
        at("2026-11-13T14:00:00Z"),
    );
    assert_eq!(*reads.borrow(), 3, "the group is read");
    let entry = pass.stalled(406).expect("found");
    assert!(
        matches!(&entry.verdict, Verdict::Stalled { doomed: Some(failed), .. } if failed.context == "framework-trusted-verify"),
        "{:?}",
        entry.verdict
    );
}
