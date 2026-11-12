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
    assert_eq!(
        small.calls[1], large.calls[1],
        "steady state: 2 blocked -> {} calls, 12 blocked -> {} calls",
        small.calls[1], large.calls[1]
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
