//! Dwell-time limits and the next-to-admit ranking inside a steward pass.
use std::cell::RefCell;

use super::*;
use crate::app::merge_steward_cmd::blocked_on::Stall;
use crate::app::merge_steward_cmd::carried_commits::{CommitList, OpenPr};
use crate::app::merge_steward_cmd::dwell_pass::{self, ActOutcome, DwellActs, DwellInput};
use crate::app::merge_steward_cmd::next_action::{Actor, NextAction};
use crate::merge_steward::dwell::{ActMode, Disposition, DwellPolicy, DwellState};
use crate::merge_steward::next_admit::Tier;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-09T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn ago(minutes: i64) -> String {
    (now() - chrono::Duration::minutes(minutes)).to_rfc3339()
}

fn check(name: &str, status: &str, conclusion: Option<&str>, run: u64) -> Value {
    json!({"__typename": "CheckRun", "name": name, "status": status,
           "conclusion": conclusion, "completedAt": "2026-11-09T11:00:00Z",
           "detailsUrl": format!("https://github.com/owner/repo/actions/runs/{run}")})
}

/// `macos` and `review-verdict` in the given states.
fn pr(number: u64, macos: Option<&str>, verdict: Option<&str>, labels: &[&str]) -> ObservedPr {
    let mut rollup = Vec::new();
    match macos {
        Some("PENDING") => rollup.push(check("macos", "IN_PROGRESS", None, 100 + number)),
        Some(conclusion) => {
            rollup.push(check("macos", "COMPLETED", Some(conclusion), 100 + number));
        }
        None => {}
    }
    if let Some(conclusion) = verdict {
        rollup.push(check(
            "review-verdict",
            "COMPLETED",
            Some(conclusion),
            900 + number,
        ));
    }
    parse_pr(
        &json!({
            "id": format!("PR_{number}"), "number": number, "isDraft": false,
            "baseRefName": "main", "headRefOid": HEAD, "headRefName": format!("branch-{number}"),
            "mergeStateStatus": "CLEAN", "autoMergeRequest": null,
            "author": {"login": "app/shipyard-local"},
            "labels": labels.iter().map(|name| json!({"name": name})).collect::<Vec<_>>(),
            "statusCheckRollup": rollup,
        }),
        &BTreeMap::new(),
    )
    .expect("PR")
}

fn policy() -> StewardPolicy {
    StewardPolicy {
        required_checks: vec![
            RequiredCheck {
                context: "macos".to_owned(),
                app_id: None,
            },
            RequiredCheck {
                context: "review-verdict".to_owned(),
                app_id: None,
            },
        ],
        ..queue_policy()
    }
}

fn report(number: u64, token: &str, since_minutes: Option<i64>) -> PrReport {
    PrReport {
        number,
        head_sha: HEAD.to_owned(),
        decision: StewardDecision::ArmMergeQueue,
        mutation: None,
        error: None,
        blocked: Stall {
            blocked_on: token.to_owned(),
            since: since_minutes.map(ago),
            next_action: Some(NextAction {
                actor: Actor::Steward,
                action: format!("next for #{number}"),
                stop: false,
                escalate: None,
            }),
            ..Stall::default()
        },
    }
}

#[derive(Default)]
struct FakeActs {
    calls: RefCell<Vec<String>>,
    /// Dispatch reports a failure after its side effect was attempted.
    dispatch_fails: bool,
    /// The ledger file, read at the moment of each side effect.
    ledger_path: Option<std::path::PathBuf>,
    intent_on_disk: RefCell<Vec<bool>>,
}

impl DwellActs for FakeActs {
    fn rerun_verdict(&self, pr: &ObservedPr, _ledger: &StewardLedger) -> ActOutcome {
        if let Some(path) = &self.ledger_path {
            let saved = fs::read_to_string(path).unwrap_or_default();
            self.intent_on_disk
                .borrow_mut()
                .push(saved.contains("\"intent\""));
        }
        self.calls
            .borrow_mut()
            .push(format!("rerun #{}", pr.fact.number));
        ActOutcome::Done("re-ran".to_owned())
    }
    fn dispatch(&self, pr: &ObservedPr, workflows: &[String]) -> ActOutcome {
        self.calls.borrow_mut().push(format!(
            "dispatch #{} {}",
            pr.fact.number,
            workflows.join(",")
        ));
        if self.dispatch_fails {
            return ActOutcome::Failed("second workflow: HTTP 502".to_owned());
        }
        ActOutcome::Done("dispatched".to_owned())
    }
    fn retarget(&self, pr: &OpenPr) -> ActOutcome {
        self.calls
            .borrow_mut()
            .push(format!("retarget #{}", pr.number));
        ActOutcome::Done("retargeted".to_owned())
    }
}

struct Pass<'a> {
    observation: RepoObservation,
    reports: Vec<PrReport>,
    dwell_policy: DwellPolicy,
    listing: Option<Vec<OpenPr>>,
    apply: bool,
    temp: &'a tempfile::TempDir,
    now: DateTime<Utc>,
}

impl Pass<'_> {
    fn run(&self, ledger: &mut StewardLedger, acts: &FakeActs) -> dwell_pass::DwellRepoReport {
        let policy = policy();
        let state_dir = self.temp.path().join("state");
        dwell_pass::run(
            &DwellInput {
                observation: &self.observation,
                policy: &policy,
                dwell_policy: &self.dwell_policy,
                listing: self.listing.as_deref(),
                apply: self.apply,
                ledger_path: &self.temp.path().join("ledger.json"),
                state_dir: Some(&state_dir),
                now: self.now,
                deadline: None,
            },
            &self.reports,
            ledger,
            acts,
        )
    }
}

/// A ledger for a repository the steward already tracks: its first pass
/// seeds the backlog silently.
fn established() -> StewardLedger {
    let mut ledger = StewardLedger::default();
    ledger
        .dwell
        .entry("owner/repo@main".to_owned())
        .or_default()
        .initialized = true;
    ledger
}

fn pass(temp: &tempfile::TempDir, prs: Vec<ObservedPr>, reports: Vec<PrReport>) -> Pass<'_> {
    let mut observation = observation_for(prs[0].clone(), true);
    observation.prs = prs;
    Pass {
        observation,
        reports,
        dwell_policy: DwellPolicy {
            act: ActMode::Apply,
            ..DwellPolicy::default()
        },
        listing: None,
        apply: true,
        temp,
        now: now(),
    }
}

#[test]
fn ranking_puts_stack_bases_then_approved_green_pending_and_review_and_names_one_action() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![
        pr(1, Some("SUCCESS"), Some("SUCCESS"), &[]),
        pr(2, Some("PENDING"), Some("SUCCESS"), &[]),
        pr(3, Some("SUCCESS"), None, &[]),
        pr(4, Some("SUCCESS"), Some("SUCCESS"), &[]),
        pr(5, Some("PENDING"), Some("SUCCESS"), &[]),
        pr(6, Some("FAILURE"), None, &["stacked:5"]),
        pr(7, Some("SUCCESS"), Some("SUCCESS"), &[]),
    ];
    let mut reports = vec![
        report(1, "ready", Some(3)),
        report(2, "required_pending", Some(3)),
        report(3, "no_verdict", Some(3)),
        report(4, "merge_state:DIRTY", Some(3)),
        report(5, "required_pending", Some(3)),
        report(6, "required_failed:macos", Some(3)),
        report(7, "queued", Some(3)),
    ];
    reports[6].decision = StewardDecision::Queued { position: 1 };
    let pass = pass(&temp, prs, reports);
    let mut ledger = established();
    let report = pass.run(&mut ledger, &FakeActs::default());
    assert_eq!(
        report.ranking,
        vec![5, 1, 2, 3, 4, 6],
        "queued #7 is not ranked"
    );
    let tiers: Vec<Tier> = report.next_to_admit.iter().map(|row| row.tier).collect();
    assert_eq!(
        tiers,
        vec![
            Tier::StackBase,
            Tier::ApprovedGreen,
            Tier::ApprovedPending,
            Tier::GreenNeedsReview,
            Tier::Rest
        ]
    );
    assert_eq!(report.next_to_admit.len(), 5);
    assert_eq!(report.next_to_admit[0].unblocks, 1);
    assert_eq!(report.next_to_admit[1].action, "next for #1");
    assert_eq!(
        ledger.dwell["owner/repo@main"].admission_rank,
        vec![5, 1, 2, 3, 4, 6],
        "the next pass admits in this order"
    );
}

#[test]
fn the_next_pass_evaluates_overstayed_then_ranked_candidates_first() {
    let prs: Vec<ObservedPr> = (1..=4)
        .map(|n| pr(n, Some("SUCCESS"), Some("SUCCESS"), &[]))
        .collect();
    let decisions = vec![StewardDecision::ArmMergeQueue; 4];
    let mut ledger = established();
    let state = ledger
        .dwell
        .entry("owner/repo@main".to_owned())
        .or_default();
    state.admission_rank = vec![3, 1, 4, 2];
    state.readmit_first = vec![2];
    let order: Vec<u64> =
        pass_budget::processing_order("Owner/Repo", "main", &prs, &decisions, &ledger)
            .into_iter()
            .map(|index| prs[index].fact.number)
            .collect();
    assert_eq!(order, vec![2, 3, 1, 4]);
}

#[test]
fn a_stale_verdict_check_is_rerun_once_with_the_intent_saved_first() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![pr(1, Some("SUCCESS"), Some("FAILURE"), &[])];
    let mut pass = pass(
        &temp,
        prs,
        vec![report(1, "required_failed:review-verdict", Some(15))],
    );
    let mut ledger = established();
    let acts = FakeActs {
        ledger_path: Some(temp.path().join("ledger.json")),
        ..FakeActs::default()
    };
    let first = pass.run(&mut ledger, &acts);
    assert_eq!(*acts.calls.borrow(), vec!["rerun #1".to_owned()]);
    assert_eq!(
        *acts.intent_on_disk.borrow(),
        vec![true],
        "the intent was on disk when the side effect ran"
    );
    assert_eq!(first.overdue[0].state, DwellState::StaleVerdictCheck);
    assert!(
        !first.overdue[0].escalated,
        "the act runs before any escalation"
    );
    let saved = fs::read_to_string(temp.path().join("ledger.json")).expect("intent persisted");
    assert!(
        saved.contains("\"intent\""),
        "the intent is durable before the side effect"
    );
    // Later passes, the check still red at the same head: never again.
    pass.now = now() + chrono::Duration::minutes(30);
    let second = pass.run(&mut ledger, &acts);
    pass.now = now() + chrono::Duration::minutes(90);
    let _ = pass.run(&mut ledger, &acts);
    assert_eq!(acts.calls.borrow().len(), 1, "{:?}", acts.calls.borrow());
    assert!(matches!(second.overdue[0].act, Disposition::Acted { .. }));
    assert!(
        second.overdue[0].escalated,
        "an act that did not clear it escalates"
    );
}

#[test]
fn a_crash_after_the_intent_never_repeats_the_act() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![pr(1, Some("SUCCESS"), Some("FAILURE"), &[])];
    let pass = pass(
        &temp,
        prs,
        vec![report(1, "required_failed:review-verdict", Some(15))],
    );
    let mut ledger = established();
    pass.run(&mut ledger, &FakeActs::default());
    // Reload what was saved before the side effect, as a restarted steward.
    let mut reloaded: StewardLedger =
        serde_json::from_str(&fs::read_to_string(temp.path().join("ledger.json")).unwrap())
            .unwrap();
    let acts = FakeActs::default();
    pass.run(&mut reloaded, &acts);
    assert!(acts.calls.borrow().is_empty());
}

#[test]
fn never_acts_on_dirty_held_or_carried_heads_and_escalates_them_to_the_feed() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![
        pr(1, Some("SUCCESS"), Some("SUCCESS"), &[]),
        pr(2, Some("SUCCESS"), Some("SUCCESS"), &[]),
        pr(3, Some("SUCCESS"), Some("SUCCESS"), &[]),
    ];
    let reports = vec![
        report(1, "merge_state:DIRTY", Some(10_000)),
        report(2, "held", Some(10_000)),
        report(3, "carries_unapproved_commits:#9", Some(10_000)),
    ];
    let pass = pass(&temp, prs, reports);
    let mut ledger = established();
    let acts = FakeActs::default();
    let report = pass.run(&mut ledger, &acts);
    assert!(acts.calls.borrow().is_empty(), "{:?}", acts.calls.borrow());
    assert!(ledger.dwell["owner/repo@main"].readmit_first.is_empty());
    assert!(ledger.dwell["owner/repo@main"].acts.is_empty());
    assert_eq!(report.overdue.len(), 3);
    assert!(
        report
            .overdue
            .iter()
            .all(|row| row.act == Disposition::NoAct && row.escalated)
    );
    let feed = fs::read_to_string(temp.path().join("state/steward-dwell/overdue.jsonl")).unwrap();
    assert_eq!(feed.lines().count(), 3);
    // Each level goes to the feed once.
    pass.run(&mut ledger, &acts);
    let feed = fs::read_to_string(temp.path().join("state/steward-dwell/overdue.jsonl")).unwrap();
    assert_eq!(
        feed.lines().count(),
        3,
        "no repeat escalation at the same level"
    );
}

#[test]
fn a_dry_run_takes_no_github_act_and_writes_no_feed() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![pr(1, Some("SUCCESS"), Some("FAILURE"), &[])];
    let mut pass = pass(
        &temp,
        prs,
        vec![report(1, "required_failed:review-verdict", Some(15))],
    );
    pass.apply = false;
    let acts = FakeActs::default();
    let report = pass.run(&mut established(), &acts);
    assert!(acts.calls.borrow().is_empty());
    assert!(
        report.overdue[0]
            .act_outcome
            .as_deref()
            .unwrap()
            .contains("dry run")
    );
    assert!(
        !temp
            .path()
            .join("state/steward-dwell/overdue.jsonl")
            .exists()
    );
}

#[test]
fn shadow_mode_takes_no_github_act_but_readmits_an_approved_green_head() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![
        pr(1, Some("SUCCESS"), Some("FAILURE"), &[]),
        pr(2, Some("SUCCESS"), Some("SUCCESS"), &[]),
    ];
    let mut pass = pass(
        &temp,
        prs,
        vec![
            report(1, "required_failed:review-verdict", Some(15)),
            report(2, "ready", Some(6)),
        ],
    );
    pass.dwell_policy.act = ActMode::Shadow;
    let mut ledger = established();
    let acts = FakeActs::default();
    let report = pass.run(&mut ledger, &acts);
    assert!(acts.calls.borrow().is_empty());
    assert_eq!(ledger.dwell["owner/repo@main"].readmit_first, vec![2]);
    let row = |n: u64| report.overdue.iter().find(|row| row.pr == n).unwrap();
    assert!(matches!(row(1).act, Disposition::WouldAct { .. }));
    assert!(row(1).escalated);
}

#[test]
fn a_never_ran_check_is_dispatched_once_from_the_configured_map() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![pr(1, None, Some("SUCCESS"), &[])];
    let mut pass = pass(
        &temp,
        prs,
        vec![report(1, "required_never_ran:macos", Some(31))],
    );
    pass.dwell_policy
        .dispatch
        .insert("macos".to_owned(), "build.yml".to_owned());
    let mut ledger = established();
    let acts = FakeActs::default();
    pass.run(&mut ledger, &acts);
    pass.run(&mut ledger, &acts);
    assert_eq!(
        *acts.calls.borrow(),
        vec!["dispatch #1 build.yml".to_owned()]
    );
}

fn stacked(number: u64, base_ref: &str, parent: u64) -> OpenPr {
    OpenPr {
        number,
        head_ref: format!("branch-{number}"),
        base_ref: base_ref.to_owned(),
        head_sha: HEAD.to_owned(),
        cross_repository: false,
        author: None,
        commits: CommitList::Complete(std::collections::BTreeSet::new()),
        stacked_on: Some(parent),
    }
}

#[test]
fn a_pull_request_stacked_on_a_merged_base_is_retargeted_once() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![pr(1, Some("SUCCESS"), Some("SUCCESS"), &[])];
    let mut pass = pass(&temp, prs, vec![report(1, "ready", Some(1))]);
    // #20 sits on #7's branch; #7 is no longer open. #21 waits on open #1.
    pass.listing = Some(vec![stacked(20, "branch-7", 7), stacked(21, "branch-1", 1)]);
    let mut ledger = established();
    let acts = FakeActs::default();
    let first = pass.run(&mut ledger, &acts);
    assert!(
        acts.calls.borrow().is_empty(),
        "first sighting is not yet overdue"
    );
    assert_eq!(first.stack_listing, "reused");
    assert_eq!(first.next_to_admit[0].unblocks, 1, "#21 waits on #1");
    pass.now = now() + chrono::Duration::minutes(11);
    pass.run(&mut ledger, &acts);
    // A pass without the listing keeps the stacked entry, and does not act.
    pass.listing = None;
    pass.now = now() + chrono::Duration::minutes(40);
    pass.run(&mut ledger, &acts);
    assert!(ledger.dwell["owner/repo@main"].entries.contains_key(&20));
    assert_eq!(*acts.calls.borrow(), vec!["retarget #20".to_owned()]);
}

#[test]
fn an_owned_pull_request_escalates_through_the_relay_route() {
    let temp = tempfile::tempdir().unwrap();
    let state_dir = temp.path().join("state");
    crate::owner_registry::save(
        &state_dir,
        &crate::owner_registry::OwnerRecord {
            schema: 1,
            repo: "owner/repo".to_owned(),
            pr: 1,
            host: "m5".to_owned(),
            client: crate::owner_registry::Client::Codex,
            cmux_socket: None,
            surface: Some("1000130019".to_owned()),
            agent: None,
            session: Some("session-1".to_owned()),
            registered_at: now(),
            source: crate::owner_registry::Source::Ship,
            released: false,
        },
    )
    .expect("owner record");
    let prs = vec![
        pr(1, Some("SUCCESS"), Some("SUCCESS"), &[]),
        pr(2, Some("SUCCESS"), Some("SUCCESS"), &[]),
    ];
    let pass = pass(
        &temp,
        prs,
        vec![report(1, "held", Some(31)), report(2, "held", Some(31))],
    );
    let report = pass.run(&mut established(), &FakeActs::default());
    let row = |n: u64| report.overdue.iter().find(|row| row.pr == n).unwrap();
    assert_eq!(row(1).owner.route, "relay");
    assert_eq!(row(1).owner.surface.as_deref(), Some("1000130019"));
    assert_eq!(row(2).owner.route, "feed");
    assert_eq!(row(2).owner.role, "author");
}

#[test]
fn a_row_the_pass_budget_deferred_keeps_its_dwell() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![pr(1, Some("SUCCESS"), Some("SUCCESS"), &[])];
    let mut pass = pass(&temp, prs.clone(), vec![report(1, "held", Some(25))]);
    let mut ledger = established();
    pass.run(&mut ledger, &FakeActs::default());
    let entered = ledger.dwell["owner/repo@main"].entries[&1].entered_at;
    pass.reports = vec![pass_budget::deferred_report(
        &prs[0],
        StewardDecision::ArmMergeQueue,
        "budget",
    )];
    pass.now = now() + chrono::Duration::minutes(10);
    let report = pass.run(&mut ledger, &FakeActs::default());
    assert_eq!(
        ledger.dwell["owner/repo@main"].entries[&1].entered_at,
        entered
    );
    assert_eq!(report.overdue.len(), 1, "still overdue while deferred");
}

fn approval_rule() -> RequiredCheck {
    RequiredCheck {
        context: "review-verdict".to_owned(),
        app_id: None,
    }
}

/// A fake `gh` that logs argv and answers `pulls/1` as an open pull request
/// at `HEAD` whose head branch lives in `head_repo`.
fn live_pr_script(log: &std::path::Path, head_repo: &str) -> String {
    format!(
        "printf '%s\\n' \"$*\" >> '{log}'\n\
         case \"$*\" in\n\
           *pulls/1*) printf '%s' '{{\"state\":\"open\",\"head\":{{\"sha\":\"{HEAD}\",\"repo\":{{\"full_name\":\"{head_repo}\"}}}}}}';;\n\
           *) printf '%s' '{{}}';;\n\
         esac",
        log = log.display()
    )
}

#[cfg(unix)]
#[test]
fn github_acts_dispatch_through_the_guard_and_never_arm_merge_or_update() {
    let temp = tempfile::tempdir().unwrap();
    let approval = approval_rule();
    let log = temp.path().join("gh.log");
    let actions = fake_gh(&temp, &live_pr_script(&log, "owner/repo"));
    let control = mutation_control(&temp, "m3", "m3");
    let observed = pr(1, None, Some("SUCCESS"), &[]);
    let observation = observation_for(observed.clone(), true);
    let acts = dwell_pass::GitHubDwellActs {
        actions: &actions,
        observation: &observation,
        control: Some(&control),
        approval: Some(&approval),
    };
    let outcome = acts.dispatch(&observed, &["build.yml".to_owned()]);
    assert!(matches!(outcome, ActOutcome::Done(_)), "{outcome:?}");
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains("workflow run build.yml --ref branch-1 --repo owner/repo"),
        "{calls}"
    );
    for forbidden in ["merge", "auto-merge", "enqueue", "update-branch", "--admin"] {
        assert!(!calls.contains(forbidden), "{forbidden} in {calls}");
    }
}

#[cfg(unix)]
#[test]
fn retarget_refuses_when_the_parent_closed_without_merging() {
    let temp = tempfile::tempdir().unwrap();
    let approval = approval_rule();
    let log = temp.path().join("gh.log");
    let body = format!(
        "printf '%s\\n' \"$*\" >> '{log}'\n\
         case \"$*\" in\n\
           *pulls/20*) printf '%s' '{{\"state\":\"open\",\"base\":{{\"ref\":\"branch-7\"}},\"head\":{{\"sha\":\"{HEAD}\",\"repo\":{{\"full_name\":\"owner/repo\"}}}}}}';;\n\
           *pulls/7*) printf '%s' '{{\"state\":\"closed\",\"merged_at\":null}}';;\n\
           *) printf '%s' '{{}}';;\n\
         esac",
        log = log.display()
    );
    let actions = fake_gh(&temp, &body);
    let control = mutation_control(&temp, "m3", "m3");
    let observation = observation_for(pr(1, None, None, &[]), true);
    let acts = dwell_pass::GitHubDwellActs {
        actions: &actions,
        observation: &observation,
        control: Some(&control),
        approval: Some(&approval),
    };
    let outcome = acts.retarget(&stacked(20, "branch-7", 7));
    assert!(
        matches!(outcome, ActOutcome::Refused(ref why) if why.contains("closed without merging")),
        "{outcome:?}"
    );
    assert!(!fs::read_to_string(&log).unwrap().contains("PATCH"));
}

#[cfg(unix)]
#[test]
fn a_fork_pull_request_is_never_dispatched() {
    let temp = tempfile::tempdir().unwrap();
    let approval = approval_rule();
    let log = temp.path().join("gh.log");
    let actions = fake_gh(&temp, &live_pr_script(&log, "someone/fork"));
    let control = mutation_control(&temp, "m3", "m3");
    let observed = pr(1, None, Some("SUCCESS"), &[]);
    let observation = observation_for(observed.clone(), true);
    let acts = dwell_pass::GitHubDwellActs {
        actions: &actions,
        observation: &observation,
        control: Some(&control),
        approval: Some(&approval),
    };
    let outcome = acts.dispatch(&observed, &["build.yml".to_owned()]);
    assert!(
        matches!(outcome, ActOutcome::Refused(ref why) if why.contains("fork")),
        "{outcome:?}"
    );
    assert!(!fs::read_to_string(&log).unwrap().contains("workflow run"));
}

#[test]
fn live_revalidation_refuses_a_moved_closed_or_fork_head() {
    let live = |state: &str, sha: &str, repo: &str| json!({"state": state, "head": {"sha": sha, "repo": {"full_name": repo}}});
    assert!(
        dwell_pass::live_pr_matches(&live("open", HEAD, "Owner/Repo"), "owner/repo", HEAD).is_ok()
    );
    assert!(
        dwell_pass::live_pr_matches(&live("closed", HEAD, "owner/repo"), "owner/repo", HEAD)
            .is_err()
    );
    assert!(
        dwell_pass::live_pr_matches(&live("open", "bbbb", "owner/repo"), "owner/repo", HEAD)
            .is_err()
    );
    assert!(
        dwell_pass::live_pr_matches(&live("open", HEAD, "fork/repo"), "owner/repo", HEAD).is_err()
    );
}

#[test]
fn retarget_requires_the_parent_to_have_merged_the_exact_branch_into_the_base() {
    let stacked = stacked(20, "branch-7", 7);
    let live = json!({"state": "open", "base": {"ref": "branch-7"},
                      "head": {"sha": HEAD, "repo": {"full_name": "owner/repo"}}});
    let parent = |merged: bool, head_ref: &str, base: &str| {
        json!({"number": 7, "merged_at": if merged { json!("2026-11-09T10:00:00Z") } else { Value::Null },
               "head": {"ref": head_ref, "sha": "cccc", "repo": {"full_name": "owner/repo"}},
               "base": {"ref": base}})
    };
    let ok = parent(true, "branch-7", "main");
    let allowed = |parent: &Value, tip: Option<&str>| {
        dwell_pass::retarget_allowed(&stacked, &live, parent, tip, "owner/repo", "main")
    };
    assert_eq!(allowed(&ok, Some("cccc")), Ok(()));
    assert!(
        allowed(&parent(false, "branch-7", "main"), Some("cccc"))
            .unwrap_err()
            .contains("closed without merging")
    );
    assert!(
        allowed(&parent(true, "other-branch", "main"), Some("cccc"))
            .unwrap_err()
            .contains("was not the pull request")
    );
    assert!(
        allowed(&parent(true, "branch-7", "release/1"), Some("cccc"))
            .unwrap_err()
            .contains("not `main`")
    );
    assert!(
        allowed(&ok, Some("dddd"))
            .unwrap_err()
            .contains("moved after"),
        "unlanded commits on the base branch"
    );
    assert!(
        allowed(&ok, None).is_err(),
        "a deleted base branch is GitHub's to retarget"
    );
    let moved = json!({"state": "open", "base": {"ref": "branch-x"},
                       "head": {"sha": HEAD, "repo": {"full_name": "owner/repo"}}});
    assert!(
        dwell_pass::retarget_allowed(&stacked, &moved, &ok, Some("cccc"), "owner/repo", "main")
            .is_err()
    );
}

fn verdict_check(
    status: &str,
    conclusion: Option<&str>,
    run: u64,
    id: u64,
) -> crate::merge_steward::StewardCheck {
    crate::merge_steward::StewardCheck {
        name: "review-verdict".to_owned(),
        source: crate::merge_steward::StewardCheckSource::CheckRun,
        app_id: None,
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        run_id: Some(run),
        observed_at: None,
        check_id: Some(id),
        description: None,
    }
}

#[test]
fn only_a_completed_red_newest_verdict_run_is_rerun() {
    let rule = approval_rule();
    let red = verdict_check("COMPLETED", Some("FAILURE"), 10, 1);
    assert_eq!(
        dwell_pass::rerun_target(std::slice::from_ref(&red), &rule),
        Ok(10)
    );
    let running = verdict_check("IN_PROGRESS", None, 11, 2);
    assert!(
        dwell_pass::rerun_target(&[red.clone(), running], &rule)
            .unwrap_err()
            .contains("still running")
    );
    let green = verdict_check("COMPLETED", Some("SUCCESS"), 12, 3);
    assert!(
        dwell_pass::rerun_target(&[red, green], &rule)
            .unwrap_err()
            .contains("not red")
    );
    assert!(dwell_pass::rerun_target(&[], &rule).is_err());
}

#[test]
fn only_an_exact_head_approval_allows_a_verdict_rerun() {
    use crate::head_approval::{Approval, ApprovalBlocker, HeadGate};
    let approved = HeadGate::Approved {
        approval: Approval::Review {
            login: "reviewer".to_owned(),
            performed_via_github_app_id: None,
        },
    };
    assert_eq!(dwell_pass::gate_allows_rerun(&approved, None), Ok(()));
    assert!(
        dwell_pass::gate_allows_rerun(&HeadGate::NotRequired, None).is_err(),
        "not required is not approved"
    );
    let hold = ApprovalBlocker::Held {
        holder: "infra-review2".to_owned(),
        record: "comment",
        id: Some(1),
        at: None,
    };
    assert!(
        dwell_pass::gate_allows_rerun(&approved, Some(&hold))
            .unwrap_err()
            .contains("held")
    );
}

#[test]
fn a_deferred_row_is_never_acted_on() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![pr(1, None, Some("SUCCESS"), &[])];
    let mut pass = pass(
        &temp,
        prs.clone(),
        vec![report(1, "required_never_ran:macos", Some(31))],
    );
    pass.dwell_policy
        .dispatch
        .insert("macos".to_owned(), "build.yml".to_owned());
    pass.apply = false;
    let mut ledger = established();
    pass.run(&mut ledger, &FakeActs::default());
    pass.apply = true;
    pass.reports = vec![pass_budget::deferred_report(
        &prs[0],
        StewardDecision::ArmMergeQueue,
        "budget",
    )];
    let acts = FakeActs::default();
    let report = pass.run(&mut ledger, &acts);
    assert!(acts.calls.borrow().is_empty());
    assert!(
        matches!(report.overdue[0].act, Disposition::ActLater { .. }),
        "{:?}",
        report.overdue[0].act
    );
    assert!(
        ledger.dwell["owner/repo@main"].acts.is_empty(),
        "nothing recorded, so it runs once classified"
    );
}

#[test]
fn a_dwell_state_for_another_base_is_left_alone() {
    let temp = tempfile::tempdir().unwrap();
    let mut pass = pass(
        &temp,
        vec![pr(1, Some("SUCCESS"), Some("SUCCESS"), &[])],
        vec![report(1, "held", Some(40))],
    );
    let mut ledger = established();
    pass.run(&mut ledger, &FakeActs::default());
    pass.observation.base = "release/1".to_owned();
    pass.reports = Vec::new();
    pass.observation.prs = Vec::new();
    pass.run(&mut ledger, &FakeActs::default());
    assert!(ledger.dwell["owner/repo@main"].entries.contains_key(&1));
}

#[test]
fn a_dispatch_that_failed_after_its_attempt_is_never_sent_again_at_that_head() {
    let temp = tempfile::tempdir().unwrap();
    let prs = vec![pr(1, None, Some("SUCCESS"), &[])];
    let mut pass = pass(
        &temp,
        prs,
        vec![report(1, "required_never_ran:macos,linux", Some(31))],
    );
    pass.dwell_policy
        .dispatch
        .insert("macos".to_owned(), "build.yml".to_owned());
    pass.dwell_policy
        .dispatch
        .insert("linux".to_owned(), "linux.yml".to_owned());
    let mut ledger = established();
    let acts = FakeActs {
        dispatch_fails: true,
        ..FakeActs::default()
    };
    let first = pass.run(&mut ledger, &acts);
    assert!(first.overdue[0].escalated, "a failed act escalates at once");
    for minutes in [20, 50, 120, 300] {
        pass.now = now() + chrono::Duration::minutes(minutes);
        pass.run(&mut ledger, &acts);
    }
    assert_eq!(
        *acts.calls.borrow(),
        vec!["dispatch #1 build.yml,linux.yml".to_owned()]
    );
}

#[test]
fn a_seeded_backlog_is_not_reported_as_emitted_to_the_relay() {
    let temp = tempfile::tempdir().unwrap();
    let pass = pass(
        &temp,
        vec![pr(1, Some("SUCCESS"), Some("SUCCESS"), &[])],
        vec![report(1, "held", Some(500))],
    );
    let mut fresh = StewardLedger::default();
    let report = pass.run(&mut fresh, &FakeActs::default());
    assert!(!report.overdue[0].escalated);
    assert_eq!(
        report.overdue[0].escalated_level, 0,
        "seeded, never emitted"
    );
}
