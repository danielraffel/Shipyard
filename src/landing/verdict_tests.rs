//! Landing verdict: the line an agent quotes, pinned per state.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{Value, json};

use super::verdict::{self, RequiredState, VerdictFacts, VerdictState};
use super::verdict_gather;
use crate::gate_cost::ReadCache;
use crate::pr_queue_state::PrQueueState;
use crate::pr_watch::{CheckFact, GroupRun, HeadFact, PrHistory, RepoHistory, Thresholds};

const MACOS: &str = "macos";
const FREEZE: &str = "Vellum freeze";
const SYNC: &str = "Enforce version & skill sync";
const DOCS: &str = "Docs lint";

fn t(hour: i64, minute: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 29, 0, 0, 0).unwrap()
        + Duration::hours(hour)
        + Duration::minutes(minute)
}

fn sha(tag: &str) -> String {
    format!("{tag:0<40}")
}

fn done(id: u64, name: &str, conclusion: &str, at: DateTime<Utc>, sigs: &[&str]) -> CheckFact {
    CheckFact {
        name: name.to_owned(),
        id,
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: Some(at - Duration::minutes(20)),
        completed_at: Some(at),
        signatures: sigs.iter().map(|s| (*s).to_owned()).collect(),
        runner_name: None,
    }
}

fn running(id: u64, name: &str, status: &str, at: DateTime<Utc>) -> CheckFact {
    CheckFact {
        name: name.to_owned(),
        id,
        status: status.to_owned(),
        conclusion: None,
        started_at: Some(at),
        completed_at: None,
        signatures: Vec::new(),
        runner_name: None,
    }
}

fn head(tag: &str, seen: DateTime<Utc>, checks: Vec<CheckFact>) -> HeadFact {
    HeadFact {
        sha: sha(tag),
        first_seen_at: seen,
        gate_runs: Vec::new(),
        checks,
        merge_base: None,
    }
}

fn green(base_id: u64, at: DateTime<Utc>) -> Vec<CheckFact> {
    vec![
        done(base_id, FREEZE, "success", at, &[]),
        done(base_id + 1, SYNC, "success", at, &[]),
    ]
}

fn facts(pr: u64, heads: Vec<HeadFact>, others: Vec<PrHistory>) -> VerdictFacts {
    let required = vec![SYNC.to_owned(), FREEZE.to_owned(), MACOS.to_owned()];
    let head_sha = heads.last().map(|h| h.sha.clone()).unwrap_or_default();
    let mut prs: BTreeMap<u64, PrHistory> = others.into_iter().map(|p| (p.number, p)).collect();
    prs.insert(
        pr,
        PrHistory {
            number: pr,
            head_sha: head_sha.clone(),
            heads,
            timeline_complete: true,
            ..PrHistory::default()
        },
    );
    VerdictFacts {
        pr,
        head_sha,
        base: "main".to_owned(),
        required: required.clone(),
        history: RepoHistory {
            repo: "o/r".to_owned(),
            base: "main".to_owned(),
            from: t(-72, 0),
            to: t(12, 0),
            required_checks: required,
            prs,
            group_runs: Vec::new(),
            gaps: Vec::new(),
        },
        statuses: BTreeMap::new(),
        unreadable_logs: BTreeSet::new(),
        unread_group_runs: BTreeSet::new(),
        now: t(12, 0),
        gaps: Vec::new(),
        api_calls: 11,
    }
}

fn other_pr(number: u64, at: DateTime<Utc>, sig: &str) -> PrHistory {
    PrHistory {
        number,
        heads: vec![head(
            &format!("o{number}"),
            at - Duration::minutes(30),
            vec![done(number * 10, MACOS, "failure", at, &[sig])],
        )],
        timeline_complete: true,
        ..PrHistory::default()
    }
}

fn ejected_same_head() -> PrQueueState {
    PrQueueState::Ejected {
        reason: "failed_checks".to_owned(),
        at: Some("2026-09-29T09:00:00Z".to_owned()),
        new_head_since_removal: false,
        requeues_without_new_head: 0,
    }
}

/// Two heads failing the same test: the canonical repeat.
fn repeat_facts() -> VerdictFacts {
    let mut first = green(10, t(2, 0));
    first.push(done(
        12,
        MACOS,
        "failure",
        t(2, 0),
        &["cmake-forge-catalog-install"],
    ));
    let mut second = green(20, t(6, 0));
    second.push(done(
        22,
        MACOS,
        "failure",
        t(6, 0),
        &["cmake-forge-catalog-install"],
    ));
    facts(
        8933,
        vec![
            head("cc6302b9", t(1, 0), first),
            head("fc399ea6", t(5, 0), second),
        ],
        Vec::new(),
    )
}

#[test]
fn red_with_repeat_names_both_heads() {
    let verdict = verdict::compute(
        &repeat_facts(),
        Some(&ejected_same_head()),
        &Thresholds::default(),
    );
    assert_eq!(verdict.state, VerdictState::Red);
    assert_eq!(
        verdict.line,
        "VERDICT #8933 head fc399ea6: RED — macos failed cmake-forge-catalog-install \
         (REPEAT on 2 heads: cc6302b9, fc399ea6); other required: 2 green; queue: ejected \
         failed_checks at 2026-09-29T09:00:00Z, same head"
    );
    let macos = verdict
        .required
        .iter()
        .find(|check| check.name == MACOS)
        .unwrap();
    let repeat = macos.repeat.as_ref().expect("repeat evidence");
    assert_eq!(repeat.lanes, vec!["cc6302b9", "fc399ea6"]);
    assert!(macos.shared.is_none());
    let value = serde_json::to_value(&verdict).unwrap();
    assert_eq!(value["state"], "red");
    assert_eq!(
        value["required"][2]["repeat"]["tests"][0],
        "cmake-forge-catalog-install"
    );
    assert_eq!(value["api_calls"], 11);
}

#[test]
fn red_single_failure_is_not_a_repeat() {
    let mut first = green(10, t(2, 0));
    first.push(done(12, MACOS, "success", t(2, 0), &[]));
    let mut second = green(20, t(6, 0));
    second.push(done(
        22,
        MACOS,
        "failure",
        t(6, 0),
        &["census-drift", "b", "c", "d"],
    ));
    let facts = facts(
        7,
        vec![head("aaaa", t(1, 0), first), head("bbbb", t(5, 0), second)],
        Vec::new(),
    );
    let verdict = verdict::compute(
        &facts,
        Some(&PrQueueState::NeverArmed),
        &Thresholds::default(),
    );
    assert_eq!(verdict.state, VerdictState::Red);
    assert_eq!(
        verdict.line,
        "VERDICT #7 head bbbb0000: RED — macos failed census-drift, b, c and 1 more; other \
         required: 2 green; queue: not armed"
    );
    assert!(!verdict.line.contains("REPEAT"));
}

#[test]
fn red_with_unread_log_says_tests_are_unknown() {
    let mut checks = green(20, t(6, 0));
    checks.push(done(22, MACOS, "failure", t(6, 0), &[]));
    let mut facts = facts(7, vec![head("bbbb", t(5, 0), checks)], Vec::new());
    facts.unreadable_logs.insert(22);
    let verdict = verdict::compute(&facts, None, &Thresholds::default());
    assert!(
        verdict
            .line
            .contains("macos failed (log unread; failing tests unknown)"),
        "{}",
        verdict.line
    );
}

#[test]
fn pending_lists_what_has_not_finished() {
    let facts = facts(
        9,
        vec![head(
            "cccc",
            t(5, 0),
            vec![
                done(1, SYNC, "success", t(6, 0), &[]),
                running(2, MACOS, "in_progress", t(6, 0)),
            ],
        )],
        Vec::new(),
    );
    let armed = PrQueueState::ArmedNotQueued {
        enabled_at: None,
        requeues_without_new_head: 0,
    };
    let verdict = verdict::compute(&facts, Some(&armed), &Thresholds::default());
    assert_eq!(verdict.state, VerdictState::Pending);
    assert_eq!(
        verdict.line,
        "VERDICT #9 head cccc0000: PENDING — Vellum freeze not created, macos in_progress; \
         other required: 1 green; queue: armed"
    );
}

#[test]
fn green_needs_every_required_check_and_skipped_counts() {
    let mut checks = vec![
        done(1, SYNC, "success", t(6, 0), &[]),
        done(2, FREEZE, "skipped", t(6, 0), &[]),
    ];
    // A re-run: the earlier failed attempt is superseded by the later pass.
    checks.push(done(3, MACOS, "failure", t(5, 0), &["x"]));
    checks.push(done(4, MACOS, "success", t(7, 0), &[]));
    let facts = facts(10, vec![head("dddd", t(4, 0), checks)], Vec::new());
    let queued = PrQueueState::Queued {
        entry_state: None,
        position: Some(2),
        requeues_without_new_head: 0,
    };
    let verdict = verdict::compute(&facts, Some(&queued), &Thresholds::default());
    assert_eq!(verdict.state, VerdictState::Green);
    assert_eq!(
        verdict.line,
        "VERDICT #10 head dddd0000: GREEN — all 3 required green; queue: queued pos 2"
    );
}

#[test]
fn required_commit_status_is_read_when_no_check_run_exists() {
    let mut facts = facts(
        10,
        vec![head("dddd", t(4, 0), green(1, t(5, 0)))],
        Vec::new(),
    );
    facts
        .statuses
        .insert(MACOS.to_owned(), "failure".to_owned());
    let verdict = verdict::compute(&facts, None, &Thresholds::default());
    assert_eq!(verdict.state, VerdictState::Red);
    assert!(
        verdict
            .line
            .contains("RED — macos failed; other required: 2 green"),
        "{}",
        verdict.line
    );
}

#[test]
fn shared_failure_names_the_other_prs_instead_of_this_one() {
    let mut facts = repeat_facts();
    for (number, minute) in [(8801, 10), (8802, 20)] {
        facts.history.prs.insert(
            number,
            other_pr(number, t(10, minute), "cmake-forge-catalog-install"),
        );
    }
    let verdict = verdict::compute(&facts, None, &Thresholds::default());
    assert_eq!(verdict.state, VerdictState::Red);
    assert_eq!(
        verdict.line,
        "VERDICT #8933 head fc399ea6: RED — macos failed cmake-forge-catalog-install (also \
         failing on #8801,#8802 — likely main/shared); other required: 2 green"
    );
    let macos = &verdict.required[2];
    assert_eq!(macos.shared.as_ref().unwrap().other_prs, vec![8801, 8802]);
    assert!(macos.repeat.is_none());
}

#[test]
fn one_other_pr_is_not_shared() {
    let mut facts = repeat_facts();
    facts.history.prs.insert(
        8801,
        other_pr(8801, t(10, 0), "cmake-forge-catalog-install"),
    );
    let verdict = verdict::compute(&facts, None, &Thresholds::default());
    assert!(
        verdict.line.contains("REPEAT on 2 heads"),
        "{}",
        verdict.line
    );
    assert!(!verdict.line.contains("likely main/shared"));
}

#[test]
fn advisory_red_alone_is_not_red() {
    let mut checks = green(1, t(6, 0));
    checks.push(done(3, MACOS, "success", t(6, 0), &[]));
    checks.push(done(4, DOCS, "failure", t(6, 0), &["error: lint"]));
    let mut facts = facts(11, vec![head("eeee", t(5, 0), checks)], Vec::new());
    // The latest merge group on this head failed only an advisory job.
    facts.history.group_runs.push(GroupRun {
        id: 900,
        pr: Some(11),
        head_sha: sha("g900"),
        created_at: t(7, 0),
        conclusion: Some("failure".to_owned()),
        required_jobs: vec![done(901, DOCS, "failure", t(7, 30), &["error: lint"])],
        ..GroupRun::default()
    });
    let verdict = verdict::compute(&facts, None, &Thresholds::default());
    assert_eq!(verdict.state, VerdictState::Green, "{}", verdict.line);
    assert!(verdict.merge_group.is_none());
    assert!(!verdict.line.contains(DOCS));
}

#[test]
fn failed_merge_group_on_a_green_head_is_red() {
    let mut checks = green(1, t(6, 0));
    checks.push(done(3, MACOS, "success", t(6, 0), &[]));
    let mut facts = facts(12, vec![head("ffff", t(5, 0), checks)], Vec::new());
    for (id, hour) in [(700, 7), (710, 9)] {
        facts.history.group_runs.push(GroupRun {
            id,
            pr: Some(12),
            head_sha: sha(&format!("g{id}")),
            created_at: t(hour, 0),
            conclusion: Some("failure".to_owned()),
            required_jobs: vec![done(id + 1, MACOS, "failure", t(hour, 30), &["gpu-probe"])],
            ..GroupRun::default()
        });
    }
    let verdict = verdict::compute(&facts, Some(&ejected_same_head()), &Thresholds::default());
    assert_eq!(verdict.state, VerdictState::Red);
    assert_eq!(
        verdict.line,
        "VERDICT #12 head ffff0000: RED — merge group run 710 (named for #12, on this head) \
         failed macos gpu-probe (REPEAT on 2 runs: merge group 700, merge group 710); head \
         required: 3 green; queue: ejected failed_checks at 2026-09-29T09:00:00Z, same head"
    );
    assert!(
        verdict
            .required
            .iter()
            .all(|c| c.state == RequiredState::Green)
    );

    // A later group that passed clears it.
    facts.history.group_runs.push(GroupRun {
        id: 720,
        pr: Some(12),
        head_sha: sha("g720"),
        created_at: t(10, 0),
        conclusion: Some("success".to_owned()),
        ..GroupRun::default()
    });
    let verdict = verdict::compute(&facts, None, &Thresholds::default());
    assert_eq!(verdict.state, VerdictState::Green, "{}", verdict.line);
}

#[test]
fn unknown_verdict_refuses_to_characterise() {
    let verdict = verdict::unknown(
        5,
        Some(sha("abc")),
        "required checks of `main` unreadable: HTTP 502",
        Some("armed".to_owned()),
        Vec::new(),
        2,
    );
    assert_eq!(verdict.state, VerdictState::Unknown);
    assert_eq!(
        verdict.line,
        "VERDICT #5 head abc00000: UNKNOWN — required checks of `main` unreadable: HTTP 502; do \
         not report this PR as green, red, or flaky; queue: armed"
    );
}

/// A routing reader over inline JSON bodies, recording every argv.
fn routed(
    routes: Vec<(String, Value)>,
    calls: &std::cell::RefCell<Vec<String>>,
) -> impl Fn(&[String]) -> Result<String, String> + '_ {
    move |args: &[String]| {
        let joined = args.join(" ");
        calls.borrow_mut().push(joined.clone());
        for (needle, body) in &routes {
            if joined.contains(needle.as_str()) {
                return Ok(match body {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                });
            }
        }
        Err(format!("HTTP 404: no route for {joined}"))
    }
}

fn check_run(id: u64, name: &str, conclusion: &str, run: u64) -> Value {
    json!({
        "id": id, "name": name, "status": "completed", "conclusion": conclusion,
        "started_at": "2026-09-29T05:00:00Z", "completed_at": "2026-09-29T05:30:00Z",
        "html_url": format!("https://github.com/o/r/actions/runs/{run}/job/{id}")
    })
}

#[test]
fn gather_reads_a_bounded_set_and_ignores_advisory_failures() {
    let head_sha = sha("cur");
    let pull = json!({"data": {"repository": {
        "pullRequest": {
            "headRefOid": head_sha, "headRefName": "feature/x", "baseRefName": "main",
            "createdAt": "2026-09-28T20:00:00Z",
            "timelineItems": {"nodes": [
                {"__typename": "AddedToMergeQueueEvent", "createdAt": "2026-09-29T06:00:00Z"},
                {"__typename": "RemovedFromMergeQueueEvent", "createdAt": "2026-09-29T07:00:00Z"}
            ]}
        },
        "pullRequests": {"nodes": [{"number": 42, "headRefName": "feature/x"}]}
    }}});
    let required = json!({"contexts": [MACOS, SYNC], "checks": []});
    let head_runs = json!({"total_count": 3, "check_runs": [
        check_run(1, MACOS, "success", 500),
        check_run(2, SYNC, "success", 501),
        check_run(3, DOCS, "failure", 502),
    ]});
    let branch_runs = json!({"workflow_runs": [
        {"id": 500, "workflow_id": 9, "head_sha": head_sha, "head_branch": "feature/x",
         "created_at": "2026-09-29T04:50:00Z", "status": "completed", "conclusion": "failure"}
    ]});
    let groups = json!({"total_count": 1, "workflow_runs": [
        {"id": 600, "workflow_id": 9, "head_sha": sha("g600"),
         "head_branch": format!("gh-readonly-queue/main/pr-42-{}", sha("base")),
         "created_at": "2026-09-29T06:05:00Z", "status": "completed", "conclusion": "failure"}
    ]});
    // The group failed only an advisory job; the required one passed.
    let group_jobs = json!({"jobs": [
        check_run(601, MACOS, "success", 600),
        check_run(602, DOCS, "failure", 600),
    ]});
    let calls = std::cell::RefCell::new(Vec::new());
    let reader = routed(
        vec![
            ("graphql".to_owned(), pull),
            ("protection/required_status_checks".to_owned(), required),
            (format!("commits/{head_sha}/check-runs"), head_runs),
            ("event=pull_request&per_page".to_owned(), branch_runs),
            ("event=merge_group&created=2026".to_owned(), groups),
            ("runs/600/jobs".to_owned(), group_jobs),
        ],
        &calls,
    );
    let facts = verdict_gather::gather(&reader, &ReadCache::disabled(), "o/r", 42, t(12, 0))
        .expect("facts");
    let verdict = verdict::compute(&facts, None, &Thresholds::default());
    assert_eq!(verdict.state, VerdictState::Green, "{}", verdict.line);
    assert_eq!(
        verdict.line,
        "VERDICT #42 head cur00000: GREEN — all 2 required green"
    );
    // pull + protection + head check runs + branch runs + one queue window +
    // one failed group's jobs. No logs: nothing required failed.
    assert_eq!(verdict.api_calls, 6, "{:#?}", calls.borrow());
    assert_eq!(calls.borrow().len(), 6);
    assert!(
        calls
            .borrow()
            .iter()
            .any(|call| call.contains("created=2026-09-29T06:00:00Z..2026-09-29T07:05:00Z")),
        "{:#?}",
        calls.borrow()
    );
}

#[test]
fn gather_failure_on_required_checks_is_unknown_not_green() {
    let calls = std::cell::RefCell::new(Vec::new());
    let pull = json!({"data": {"repository": {"pullRequest": {
        "headRefOid": sha("cur"), "headRefName": "x", "baseRefName": "main",
        "createdAt": "2026-09-28T20:00:00Z", "timelineItems": {"nodes": []}}}}});
    let reader = routed(vec![("graphql".to_owned(), pull)], &calls);
    let failure = verdict_gather::gather(&reader, &ReadCache::disabled(), "o/r", 42, t(12, 0))
        .expect_err("unreadable protection");
    assert!(
        failure
            .detail
            .contains("required checks of `main` unreadable"),
        "{}",
        failure.detail
    );
    assert_eq!(failure.api_calls, 2);
}

#[test]
fn queue_suffix_covers_every_state() {
    assert_eq!(verdict::queue_suffix(&PrQueueState::Merged), "merged");
    assert_eq!(
        verdict::queue_suffix(&PrQueueState::NeverArmed),
        "not armed"
    );
    assert_eq!(
        verdict::queue_suffix(&PrQueueState::Ejected {
            reason: "failed_checks".to_owned(),
            at: None,
            new_head_since_removal: true,
            requeues_without_new_head: 0,
        }),
        "ejected failed_checks at UNKNOWN, new head since"
    );
    assert_eq!(
        verdict::queue_suffix(&PrQueueState::Unknown {
            detail: "x".to_owned()
        }),
        "UNKNOWN"
    );
}
