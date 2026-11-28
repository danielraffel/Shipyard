//! What one apply pass costs in GitHub calls, as the backlog grows.
//!
//! A pass on a busy repository is dominated by pull requests that cannot be
//! admitted: failing required checks, waiting on a fix. Once a pass has
//! published such a failure, the next pass must not pay a live revalidation
//! for it again, so the steady-state cost of a pass tracks the pull requests
//! that can move, not every open one.

use super::*;
use crate::app::merge_steward_cmd::recovery_worker::with_recovery_publication;
use crate::merge_steward::{StewardCheck, StewardCheckSource};

/// The one pull request in the fixture that can be admitted.
const ADMISSIBLE: u64 = 7;

fn head_for(number: u64) -> String {
    format!("{number:040}")
}

fn check(name: &str, source: StewardCheckSource, conclusion: &str) -> StewardCheck {
    StewardCheck {
        name: name.to_owned(),
        source,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        run_id: None,
        observed_at: Some("2026-11-08T00:00:00Z".to_owned()),
        check_id: None,
        description: None,
    }
}

/// A managed pull request; `failing` ones carry a red required check and an
/// already-signalled needs-agent recovery, as a backlog does between fixes.
fn fixture_pr(number: u64, failing: bool) -> ObservedPr {
    let mut pr = ready_pr();
    pr.node_id = format!("PR_{number}");
    pr.fact.number = number;
    pr.fact.head_sha = head_for(number);
    pr.fact.head_branch = format!("feature-{number}");
    pr.fact.labels = vec![MANAGED_LABEL.to_owned()];
    pr.fact.checks = vec![
        check(
            "macos",
            StewardCheckSource::CheckRun,
            if failing { "FAILURE" } else { "SUCCESS" },
        ),
        check(
            HANDOFF_CONTEXT,
            StewardCheckSource::StatusContext,
            "SUCCESS",
        ),
    ];
    if failing {
        pr.fact.labels.push(NEEDS_AGENT_LABEL.to_owned());
        pr.fact.checks.push(check(
            RECOVERY_CONTEXT,
            StewardCheckSource::StatusContext,
            "FAILURE",
        ));
    }
    pr
}

/// A `gh` stand-in that logs every call and answers the reads a pass makes:
/// an empty merge queue, and each pull request's live row as the census saw it.
fn fixture_gh(temp: &tempfile::TempDir, log: &Path) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    n=$3
    head=$(printf '%040d' "$n")
    if [ "$n" = "{admissible}" ]; then
      checks='{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-11-08T00:00:00Z"}}'
      labels='{{"name":"{managed}"}}'
    else
      checks='{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"FAILURE","completedAt":"2026-11-08T00:00:00Z"}},{{"__typename":"StatusContext","context":"{recovery}","state":"FAILURE","createdAt":"2026-11-08T00:00:00Z"}}'
      labels='{{"name":"{managed}"}},{{"name":"{needs_agent}"}}'
    fi
    printf '{{"id":"PR_%s","number":%s,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"%s","headRefName":"feature-%s","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[%s],"statusCheckRollup":[%s,{{"__typename":"StatusContext","context":"{handoff}","state":"SUCCESS","createdAt":"2026-11-08T00:00:00Z"}}]}}' "$n" "$n" "$head" "$n" "$labels" "$checks" ;;
  *) printf '%s' '{{}}' ;;
esac
"#,
            log = log.display(),
            admissible = ADMISSIBLE,
            managed = MANAGED_LABEL,
            needs_agent = NEEDS_AGENT_LABEL,
            recovery = RECOVERY_CONTEXT,
            handoff = HANDOFF_CONTEXT,
        ),
    )
}

fn apply_args() -> StewardCommandArgs {
    StewardCommandArgs {
        repos: vec!["owner/repo".to_owned()],
        base: "main".to_owned(),
        opt_out_label: "steward:skip".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: false,
        coalesce: false,
        preempt_capacity: false,
        max_preemptions_per_head: 1,
        arm_unqueued: false,
        approval_context: crate::merge_steward::DEFAULT_APPROVAL_CONTEXT.to_owned(),
        wake_owner: false,
        unapproved_dequeue: crate::app::merge_steward_cmd::DequeueMode::Report,
        solo_entry: crate::app::merge_steward_cmd::solo_entry::Settings::default(),
        infra_rerun: crate::app::merge_steward_cmd::InfraRerunMode::Off,
        apply: true,
        ledger: None,
        pass_budget: None,
        pass_deadline: None,
        invariants: None,
    }
}

struct PassCost {
    /// GitHub calls each pass made, in order.
    calls: Vec<usize>,
    /// The recovery outcome each failing pull request reported, per pass.
    failing_mutations: Vec<Vec<Option<String>>>,
}

/// Run `passes` apply passes over one admissible and `failing` blocked pull
/// requests, sharing one ledger and state directory as consecutive ticks do.
fn passes_over(failing: u64, passes: usize) -> PassCost {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fixture_gh(&temp, &log);
    let mut observation = observation_for(fixture_pr(ADMISSIBLE, false), true);
    observation
        .prs
        .extend((100..100 + failing).map(|number| fixture_pr(number, true)));
    let policy_decisions = observation
        .prs
        .iter()
        .map(|pr| {
            let attempts = attempts_for(&StewardLedger::default(), "owner/repo", &pr.fact);
            classify_pr(&pr.fact, &queue_policy(), &attempts)
        })
        .collect::<Vec<_>>();
    assert!(
        policy_decisions
            .iter()
            .skip(1)
            .all(|decision| matches!(decision, StewardDecision::RequiredFailed { .. })),
        "every blocked fixture is a required-check failure: {policy_decisions:?}"
    );
    let args = apply_args();
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger::default();
    let mut cost = PassCost {
        calls: Vec::new(),
        failing_mutations: Vec::new(),
    };
    with_recovery_publication(false, || {
        for _ in 0..passes {
            let before = fs::read_to_string(&log).map_or(0, |calls| calls.lines().count());
            let (report, _, _) = apply_repo_plan(
                &actions,
                &args,
                &observation,
                &ledger_path,
                &mut ledger,
                0,
                Some(&control),
                None,
            );
            let after = fs::read_to_string(&log).map_or(0, |calls| calls.lines().count());
            cost.calls.push(after - before);
            cost.failing_mutations.push(
                report
                    .prs
                    .iter()
                    .filter(|pr| pr.number != ADMISSIBLE)
                    .map(|pr| {
                        assert!(pr.error.is_none(), "{pr:?}");
                        pr.mutation.clone()
                    })
                    .collect(),
            );
        }
    });
    cost
}

#[test]
fn a_steady_state_pass_costs_the_admissible_pull_requests_not_the_backlog() {
    let small = passes_over(2, 2);
    let large = passes_over(12, 2);

    // The control: the first pass publishes each failure and must revalidate
    // it live, so it grows with the backlog. A test that saw no growth here
    // would not be measuring the revalidation at all.
    assert!(
        large.calls[0] >= small.calls[0] + 10 * 2,
        "first pass: 2 blocked -> {} calls, 12 blocked -> {} calls",
        small.calls[0],
        large.calls[0]
    );
    // Once published, a blocked pull request costs nothing per pass: ten more
    // of them add no GitHub call, and the admissible one still costs some.
    // The one exception is the single base tip read for a shared-failure
    // episode's check: the 12 blocked heads fail the same check, so that
    // episode costs one read per pass however many members it has.
    assert_eq!(
        small.calls[1] + 1,
        large.calls[1],
        "steady state: 2 blocked -> {} calls, 12 blocked -> {} calls",
        small.calls[1],
        large.calls[1]
    );
    assert!(
        small.calls[1] > 0,
        "the admissible pull request is still read"
    );
    // And the decision each blocked pull request reports is unchanged.
    for cost in [&small, &large] {
        assert!(
            cost.failing_mutations
                .iter()
                .flatten()
                .all(|mutation| mutation.as_deref() == Some("actionable_failure_persisted")),
            "{:?}",
            cost.failing_mutations
        );
    }
}

#[test]
fn the_ledger_probe_matches_what_the_write_would_do() {
    use crate::app::merge_steward_cmd::terminal_handoff::{
        actionable_failure_is_recorded, persist_actionable_failure,
    };
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger::default();
    let head = head_for(100);
    let contexts = vec!["required_check:macos:unbound:conclusion=FAILURE:run_id=1".to_owned()];
    let recorded = |ledger: &StewardLedger, contexts: Vec<String>| {
        actionable_failure_is_recorded(ledger, "owner/repo", "main", 100, &head, None, contexts)
            .expect("probe")
    };

    assert!(!recorded(&ledger, contexts.clone()), "nothing recorded yet");
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        100,
        &head,
        None,
        contexts.clone(),
    )
    .expect("persist");
    assert!(recorded(&ledger, contexts.clone()));
    // The probe never writes: the ledger on disk is still the one persisted.
    let on_disk = fs::read_to_string(&path).expect("ledger");
    assert!(recorded(&ledger, contexts.clone()));
    assert_eq!(fs::read_to_string(&path).expect("ledger"), on_disk);
    // A different failure on the same head is a new record.
    assert!(!recorded(
        &ledger,
        vec!["required_check:macos:unbound:conclusion=FAILURE:run_id=2".to_owned()]
    ));
    // A resolved record would be re-armed by the write, so it is not settled.
    for record in ledger.terminal_handoffs.values_mut() {
        record.phase = TerminalHandoffPhase::Resolved;
    }
    assert!(!recorded(&ledger, contexts));
}

#[test]
fn a_pass_spawns_a_cacheable_read_once_and_a_live_read_every_time() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let pass = std::sync::Arc::new(crate::gh_pass::GhPass::caching());
    let actions = fake_gh(
        &temp,
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'\nprintf '%s' '{{}}'",
            log.display()
        ),
    )
    .with_pass(std::sync::Arc::clone(&pass));
    let commit = vec![
        "api".to_owned(),
        format!("repos/owner/repo/commits/{}", head_for(1)),
    ];
    let reviews = vec![
        "api".to_owned(),
        "repos/owner/repo/pulls/1/reviews".to_owned(),
    ];
    for _ in 0..3 {
        actions.run_gh(&commit).expect("commit");
        actions.run_gh(&reviews).expect("reviews");
    }
    let calls = fs::read_to_string(&log).expect("calls");
    assert_eq!(calls.matches("/commits/").count(), 1, "{calls}");
    assert_eq!(calls.matches("/reviews").count(), 3, "{calls}");
    let report = pass.report();
    assert_eq!((report.rest, report.graphql, report.cache_hits), (4, 0, 2));
}

/// An observed, failing pull request whose recovery is already published.
const LIVE_FAILING: u64 = 100;

fn recovery_request_for(number: u64) -> crate::recovery_worker::RecoveryRequest {
    crate::recovery_worker::RecoveryRequest::new(
        "owner/repo",
        number,
        "main",
        head_for(number),
        "failure-v1",
        "Required macos check failed at the exact head.",
        vec![crate::recovery_worker::RecoveryRequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
        }],
        vec![crate::recovery_worker::RecoveryFailureFact::RequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
            conclusion: "FAILURE".to_owned(),
            run_id: None,
        }],
        "policy-v1",
        "config-v1",
    )
    .expect("recovery request")
}

/// Seed one record for a pull request the pass will not observe (999) and one
/// for the observed failing pull request (`LIVE_FAILING`), then run one pass over the
/// observation and return the store, the two ids and the report.
fn pass_over_seeded_store(
    publication: bool,
    apply: bool,
) -> (
    crate::recovery_worker::RecoveryStore,
    String,
    String,
    RepoReport,
    tempfile::TempDir,
) {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fixture_gh(&temp, &log);
    let mut observation = observation_for(fixture_pr(ADMISSIBLE, false), true);
    observation.prs.push(fixture_pr(LIVE_FAILING, true));
    let mut args = apply_args();
    args.apply = apply;
    let control = mutation_control(&temp, "studio", "studio");
    let store = crate::recovery_worker::RecoveryStore::new(
        control.state_dir.join("merge-steward").join("recovery"),
    )
    .expect("store");
    let gone = recovery_request_for(999);
    let live = recovery_request_for(LIVE_FAILING);
    store.enqueue(gone.clone()).expect("seed gone");
    store.enqueue(live.clone()).expect("seed live");
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger::default();
    let report = with_recovery_publication(publication, || {
        apply_repo_plan(
            &actions,
            &args,
            &observation,
            &ledger_path,
            &mut ledger,
            0,
            apply.then_some(&control),
            None,
        )
        .0
    });
    (store, gone.id, live.id, report, temp)
}

fn record_status(
    store: &crate::recovery_worker::RecoveryStore,
    id: &str,
) -> crate::recovery_worker::RecoveryStatus {
    store.get(id).expect("get").expect("record").receipt.status
}

#[test]
fn an_applying_pass_retires_the_request_of_an_unobserved_pull_request() {
    use crate::recovery_worker::RecoveryStatus::{Pending, Superseded};
    let (store, gone, live, report, _temp) = pass_over_seeded_store(true, true);
    assert_eq!(record_status(&store, &gone), Superseded);
    assert_eq!(record_status(&store, &live), Pending);
    // The shape the steward tick's jq reads: `.recovery_store.active` and
    // `.recovery_store.limit` on the repository report.
    let json = serde_json::to_value(&report).expect("report json");
    assert_eq!(json["recovery_store"]["active"], 1);
    assert_eq!(json["recovery_store"]["limit"], 1024);
    assert_eq!(json["recovery_store"]["retired_dead"], 1);
    assert!(json["recovery_store"].get("error").is_none());
}

#[test]
fn a_dry_pass_prunes_nothing_and_reports_no_store() {
    use crate::recovery_worker::RecoveryStatus::Pending;
    let (store, gone, live, report, _temp) = pass_over_seeded_store(true, false);
    assert_eq!(record_status(&store, &gone), Pending);
    assert_eq!(record_status(&store, &live), Pending);
    assert!(report.recovery_store.is_none());
}

#[test]
fn a_pass_with_publication_off_neither_prunes_nor_reports() {
    use crate::recovery_worker::RecoveryStatus::Pending;
    let (store, gone, _, report, _temp) = pass_over_seeded_store(false, true);
    assert_eq!(record_status(&store, &gone), Pending);
    assert!(report.recovery_store.is_none());
}
