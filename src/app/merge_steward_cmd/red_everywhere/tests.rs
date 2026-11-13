use std::collections::BTreeMap;

use super::*;
use crate::app::merge_steward_cmd::ObservedPr;
use crate::app::merge_steward_cmd::blocked_on::Stall;
use crate::app::merge_steward_cmd::infra_rerun::Record;
use crate::app::merge_steward_cmd::next_action::{Actor, NextAction};
use crate::merge_steward::{
    CapacityPreemptionPolicy, RequiredCheck, StewardCheckSource, StewardDecision,
    StewardPullRequest,
};

const CHECK: &str = "Enforce version & skill sync";
const T0: i64 = 1_794_500_000;

fn head(pr: u64) -> String {
    format!("{:040x}", 0xb000 + pr)
}

fn red(pr: u64, tests: &[&str]) -> Red {
    Red {
        pr,
        head: head(pr),
        check: CHECK.to_owned(),
        tests: tests.iter().map(|test| (*test).to_owned()).collect(),
    }
}

fn failed(name: &str, run_id: u64) -> StewardCheck {
    StewardCheck {
        name: name.to_owned(),
        source: StewardCheckSource::CheckRun,
        app_id: Some(15_368),
        status: "COMPLETED".to_owned(),
        conclusion: Some("FAILURE".to_owned()),
        run_id: Some(run_id),
        observed_at: None,
        check_id: Some(run_id * 10),
        description: None,
    }
}

fn observed(number: u64, head_sha: String, checks: Vec<StewardCheck>) -> ObservedPr {
    ObservedPr {
        node_id: format!("PR_{number}"),
        owner: None,
        fact: StewardPullRequest {
            number,
            head_sha,
            head_branch: format!("feature-{number}"),
            author: Some("someone".to_owned()),
            draft: false,
            merge_state: "BLOCKED".to_owned(),
            auto_merge_active: false,
            queue_position: None,
            labels: Vec::new(),
            checks,
        },
        check_rollup_maybe_truncated: false,
    }
}

fn observation(prs: Vec<ObservedPr>) -> RepoObservation {
    RepoObservation {
        repo: "Generous-Corp/pulp".to_owned(),
        base: "main".to_owned(),
        allow_auto_merge: true,
        merge_queue: true,
        required_checks: [CHECK, "review-verdict"]
            .into_iter()
            .map(|context| RequiredCheck {
                context: context.to_owned(),
                app_id: Some(15_368),
            })
            .collect(),
        prs,
        runs: Vec::new(),
        merge_group_heads: BTreeMap::new(),
        merge_group_enqueued_at: BTreeMap::new(),
        capacity_preemption_policy: CapacityPreemptionPolicy::for_repository("generous-corp/pulp"),
        preemption_error: None,
    }
}

fn report(number: u64) -> PrReport {
    PrReport {
        number,
        head_sha: head(number),
        decision: StewardDecision::RequiredFailed {
            contexts: vec![CHECK.to_owned()],
        },
        mutation: None,
        error: None,
        blocked: Stall {
            blocked_on: format!("required_failed:{CHECK}"),
            next_action: Some(NextAction {
                actor: Actor::Author,
                action: "Required check(s) failed on this head: fix, push once.".to_owned(),
                stop: false,
            }),
            ..Stall::default()
        },
    }
}

fn keys(
    groups: &BTreeMap<(String, Option<String>), BTreeMap<u64, String>>,
) -> Vec<(Option<String>, Vec<u64>)> {
    groups
        .iter()
        .map(|((_, test), members)| (test.clone(), members.keys().copied().collect()))
        .collect()
}

#[test]
fn three_prs_failing_the_same_test_form_one_group() {
    let groups = group(&[
        red(12147, &["import_validation_source_contracts"]),
        red(12152, &["import_validation_source_contracts"]),
        red(12160, &["import_validation_source_contracts", "other"]),
    ]);
    assert_eq!(
        keys(&groups),
        vec![(
            Some("import_validation_source_contracts".to_owned()),
            vec![12147, 12152, 12160]
        )]
    );
}

#[test]
fn two_prs_failing_the_same_test_do_not_group() {
    assert!(group(&[red(1, &["t"]), red(2, &["t"])]).is_empty());
}

#[test]
fn three_prs_failing_different_tests_do_not_group() {
    assert!(group(&[red(1, &["a"]), red(2, &["b"]), red(3, &["c"])]).is_empty());
}

#[test]
fn planted_negative_two_on_one_test_and_one_on_another_never_group() {
    assert!(group(&[red(1, &["a"]), red(2, &["a"]), red(3, &["b"])]).is_empty());
}

#[test]
fn heads_whose_tests_were_not_read_group_by_the_check_alone() {
    let groups = group(&[red(1, &[]), red(2, &[]), red(3, &[])]);
    assert_eq!(keys(&groups), vec![(None, vec![1, 2, 3])]);
    // An unread head never joins a named test's group.
    assert!(group(&[red(1, &["a"]), red(2, &["a"]), red(3, &[])]).is_empty());
}

fn three_red_prs() -> RepoObservation {
    observation(
        [12147_u64, 12152, 12160]
            .into_iter()
            .map(|n| observed(n, head(n), vec![failed(CHECK, n)]))
            .collect(),
    )
}

fn ledger_with_tests(obs: &RepoObservation, test: &str) -> StewardLedger {
    let mut ledger = StewardLedger::default();
    for pr in &obs.prs {
        ledger.infra_reruns.insert(
            record_key(&obs.repo, pr.fact.number, &pr.fact.head_sha, CHECK),
            Record {
                pr: pr.fact.number,
                check: CHECK.to_owned(),
                verdict: "declined".to_owned(),
                tests: vec![test.to_owned()],
                ..Record::default()
            },
        );
    }
    ledger
}

#[test]
fn shadow_reports_the_episode_and_rewords_each_member_row_only() {
    let obs = three_red_prs();
    let mut ledger = ledger_with_tests(&obs, "due_soon_fixture");
    let reds = reds(&obs, &ledger, "review-verdict", &[]);
    let mut reports = vec![report(12147), report(12152), report(12160), report(99)];
    let status = run(Mode::Shadow, &obs, &reds, &mut ledger, &mut reports, T0);
    assert_eq!(status.mode, "shadow");
    assert_eq!(status.groups.len(), 1);
    let group = &status.groups[0];
    assert_eq!(
        (group.test.as_deref(), group.prs.clone(), group.new),
        (Some("due_soon_fixture"), vec![12147, 12152, 12160], true)
    );
    assert_eq!(group.first_seen, "2026-11-12T16:13:20Z");
    for row in &reports[..3] {
        let mark = row.blocked.red_everywhere.as_ref().expect("marked");
        assert!(
            !mark.repair_skip,
            "shadow never asks the repair lane to skip"
        );
        let next = row.blocked.next_action.as_ref().expect("next action");
        assert!(
            next.action
                .starts_with("Failing on 3 PRs with the same test"),
            "{}",
            next.action
        );
        assert!(next.action.contains("do not repair this PR"));
        assert_eq!(
            next.actor,
            Actor::Author,
            "the actor is unchanged in shadow"
        );
        assert_eq!(
            row.blocked.blocked_on,
            format!("required_failed:{CHECK}"),
            "blocked_on is unchanged"
        );
    }
    assert!(reports[3].blocked.red_everywhere.is_none());
    // Shadow never holds an enqueue.
    assert_eq!(
        held(Mode::Shadow, &ledger, &obs.repo, 12147, &head(12147)),
        None
    );
    // The next pass reports the same episode with its first sighting.
    let again = run(
        Mode::Shadow,
        &obs,
        &reds,
        &mut ledger,
        &mut reports,
        T0 + 300,
    );
    assert_eq!(
        (again.groups[0].new, again.groups[0].first_seen.as_str()),
        (false, "2026-11-12T16:13:20Z")
    );
}

#[test]
fn enforce_holds_unchanged_heads_and_marks_repair_skip() {
    let obs = three_red_prs();
    let mut ledger = ledger_with_tests(&obs, "due_soon_fixture");
    let reds = reds(&obs, &ledger, "review-verdict", &[]);
    let mut reports = vec![report(12147), report(12152), report(12160)];
    run(Mode::Enforce, &obs, &reds, &mut ledger, &mut reports, T0);
    assert!(reports.iter().all(|row| {
        row.blocked
            .red_everywhere
            .as_ref()
            .is_some_and(|mark| mark.repair_skip)
    }));
    assert_eq!(
        held(Mode::Enforce, &ledger, &obs.repo, 12147, &head(12147)),
        Some(CHECK.to_owned())
    );
    // A new head is never held.
    assert_eq!(
        held(Mode::Enforce, &ledger, &obs.repo, 12147, &head(1)),
        None
    );
}

#[test]
fn a_pr_whose_head_changed_after_the_failure_leaves_the_group() {
    let obs = three_red_prs();
    let mut ledger = ledger_with_tests(&obs, "due_soon_fixture");
    let reds_now = reds(&obs, &ledger, "review-verdict", &[]);
    let mut reports = vec![report(12147), report(12152), report(12160)];
    run(
        Mode::Enforce,
        &obs,
        &reds_now,
        &mut ledger,
        &mut reports,
        T0,
    );
    // #12160 pushes a new head that is green: two remain, so the episode ends.
    let mut moved = three_red_prs();
    moved.prs[2] = observed(12160, head(7), Vec::new());
    let reds_later = reds(&moved, &ledger, "review-verdict", &[]);
    let mut reports = vec![report(12147), report(12152), report(12160)];
    let status = run(
        Mode::Enforce,
        &moved,
        &reds_later,
        &mut ledger,
        &mut reports,
        T0 + 300,
    );
    assert!(status.groups.is_empty());
    assert!(
        reports
            .iter()
            .all(|row| row.blocked.red_everywhere.is_none())
    );
    assert!(
        ledger.red_everywhere.is_empty(),
        "an episode no longer seen is dropped"
    );
    assert_eq!(
        held(Mode::Enforce, &ledger, &moved.repo, 12147, &head(12147)),
        None
    );
}

#[test]
fn a_new_red_head_does_not_inherit_the_old_heads_tests() {
    let obs = three_red_prs();
    let ledger = ledger_with_tests(&obs, "due_soon_fixture");
    let mut moved = three_red_prs();
    moved.prs[2] = observed(12160, head(7), vec![failed(CHECK, 77)]);
    let reds = reds(&moved, &ledger, "review-verdict", &[]);
    assert_eq!(
        reds.iter()
            .find(|red| red.pr == 12160)
            .map(|red| red.tests.len()),
        Some(0)
    );
    assert!(
        group(&reds).is_empty(),
        "two on the test and one unread do not group"
    );
}

#[test]
fn mode_off_produces_nothing_and_forgets_episodes() {
    let obs = three_red_prs();
    let mut ledger = ledger_with_tests(&obs, "due_soon_fixture");
    let reds = reds(&obs, &ledger, "review-verdict", &[]);
    let mut reports = vec![report(12147), report(12152), report(12160)];
    run(Mode::Shadow, &obs, &reds, &mut ledger, &mut reports, T0);
    let mut reports = vec![report(12147), report(12152), report(12160)];
    let status = run(Mode::Off, &obs, &reds, &mut ledger, &mut reports, T0 + 60);
    assert_eq!((status.mode.as_str(), status.groups.len()), ("off", 0));
    assert!(
        reports
            .iter()
            .all(|row| row.blocked.red_everywhere.is_none())
    );
    assert!(ledger.red_everywhere.is_empty());
}

#[test]
fn the_approval_context_never_groups() {
    let obs = observation(
        [1_u64, 2, 3]
            .into_iter()
            .map(|n| observed(n, head(n), vec![failed("review-verdict", n)]))
            .collect(),
    );
    assert!(reds(&obs, &StewardLedger::default(), "review-verdict", &[]).is_empty());
}

#[test]
fn the_base_config_sets_the_mode_and_defaults_to_shadow() {
    use crate::merge_steward::EjectionGuards;
    let table = |raw: &str| raw.parse::<toml::Table>().expect("toml");
    assert_eq!(
        EjectionGuards::from_table(&toml::Table::new(), true).red_everywhere,
        Mode::Shadow
    );
    for (value, want) in [
        ("off", Mode::Off),
        ("enforce", Mode::Enforce),
        ("bogus", Mode::Shadow),
    ] {
        let guards = EjectionGuards::from_table(
            &table(&format!("[auto_merge]\nred_everywhere = \"{value}\"\n")),
            true,
        );
        assert_eq!(guards.red_everywhere, want, "{value}");
    }
}
