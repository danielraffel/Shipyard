use chrono::{DateTime, Utc};
use serde_json::json;

use super::*;

const HEAD: &str = "1111111111111111111111111111111111111111";
const GROUP: &str = "2222222222222222222222222222222222222222";

fn at(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("timestamp")
        .with_timezone(&Utc)
}

fn green(context: &str) -> RequiredFact {
    RequiredFact {
        context: context.to_owned(),
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: Some(1),
    }
}

fn required(context: &str, conclusion: &str, run_id: u64) -> RequiredFact {
    RequiredFact {
        context: context.to_owned(),
        status: "COMPLETED".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        run_id: Some(run_id),
    }
}

fn run(id: u64, event: &str, head: &str, status: &str, attempt: u64) -> RunFact {
    RunFact {
        id,
        workflow: "build.yml".to_owned(),
        event: event.to_owned(),
        head_sha: head.to_owned(),
        status: status.to_owned(),
        conclusion: (status == "completed").then(|| "cancelled".to_owned()),
        run_attempt: attempt,
        run_started_at: Some(at("2026-10-09T08:00:00Z")),
        jobs: Vec::new(),
    }
}

fn job(name: &str, conclusion: &str, runner: Option<&str>) -> JobFact {
    // A no-runner cancellation waited 17 minutes: the starvation signature.
    JobFact {
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        runner_name: runner.map(str::to_owned),
        created_at: Some(at("2026-10-09T08:02:00Z")),
        completed_at: Some(at("2026-10-09T08:19:00Z")),
    }
}

fn armed() -> CarrierFacts {
    CarrierFacts {
        repo: "owner/repo".to_owned(),
        number: 7,
        head_sha: HEAD.to_owned(),
        draft: false,
        merge_state: "BLOCKED".to_owned(),
        queue: CarrierQueueFact::ArmedNotQueued,
        approved_head: true,
        approval_evidence: Some("status:shipyard/approved-head".to_owned()),
        required: vec![green("macos"), green("Enforce")],
        runs: Vec::new(),
        observed_at: at("2026-10-09T10:00:00Z"),
    }
}

fn ejected_with(jobs: Vec<JobFact>) -> CarrierFacts {
    let mut group = run(50, "merge_group", GROUP, "completed", 1);
    group.jobs = jobs;
    CarrierFacts {
        queue: CarrierQueueFact::Ejected {
            reason: "failed_checks".to_owned(),
            new_head_since: false,
            merge_group_commit: Some(GROUP.to_owned()),
        },
        runs: vec![group],
        ..armed()
    }
}

fn starved_macos() -> Vec<JobFact> {
    vec![
        job("classify", "success", Some("hosted-1")),
        job("macos", "cancelled", None),
    ]
}

fn held(plan: &CarrierPlan) -> &CarrierHold {
    match &plan.decision {
        CarrierDecision::Hold { hold } => hold,
        CarrierDecision::Propose { action } => panic!("expected a hold, got {action:?}"),
    }
}

// Negative controls: each of these must never produce an action.

#[test]
fn a_conflicting_pull_request_is_never_acted_on() {
    for state in ["DIRTY", "CONFLICTING", "dirty"] {
        let mut facts = ejected_with(starved_macos());
        facts.merge_state = state.to_owned();
        assert!(matches!(
            held(&plan(&facts)),
            CarrierHold::Conflicting { .. }
        ));
        let mut facts = armed();
        facts.merge_state = state.to_owned();
        facts.required = vec![required("macos", "CANCELLED", 9)];
        facts.runs = vec![run(9, "pull_request", HEAD, "completed", 1)];
        assert!(matches!(
            held(&plan(&facts)),
            CarrierHold::Conflicting { .. }
        ));
    }
}

#[test]
fn a_failed_required_run_is_never_redispatched() {
    for conclusion in ["FAILURE", "TIMED_OUT", "ACTION_REQUIRED"] {
        let mut facts = armed();
        facts.required = vec![required("macos", conclusion, 9)];
        facts.runs = vec![run(9, "pull_request", HEAD, "completed", 1)];
        assert_eq!(
            held(&plan(&facts)),
            &CarrierHold::RequiredFailed {
                contexts: vec!["macos".to_owned()]
            },
            "{conclusion}"
        );
    }
}

#[test]
fn a_cancelled_run_beside_a_failure_is_not_redispatched() {
    let mut facts = armed();
    facts.required = vec![
        required("macos", "CANCELLED", 9),
        required("Enforce", "FAILURE", 10),
    ];
    facts.runs = vec![run(9, "pull_request", HEAD, "completed", 1)];
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::RequiredFailed { .. }
    ));
}

#[test]
fn a_head_without_the_approval_record_is_never_rearmed() {
    let mut facts = ejected_with(starved_macos());
    facts.approved_head = false;
    assert_eq!(held(&plan(&facts)), &CarrierHold::HeadNotApproved);
}

#[test]
fn a_head_pushed_after_the_removal_is_never_rearmed() {
    let mut facts = ejected_with(starved_macos());
    facts.queue = CarrierQueueFact::Ejected {
        reason: "failed_checks".to_owned(),
        new_head_since: true,
        merge_group_commit: Some(GROUP.to_owned()),
    };
    assert_eq!(held(&plan(&facts)), &CarrierHold::HeadMovedSinceRemoval);
}

#[test]
fn an_unarmed_pull_request_is_never_acted_on_even_when_approved() {
    let mut facts = armed();
    facts.queue = CarrierQueueFact::NeverArmed;
    facts.required = vec![required("macos", "CANCELLED", 9)];
    facts.runs = vec![run(9, "pull_request", HEAD, "completed", 1)];
    assert_eq!(held(&plan(&facts)), &CarrierHold::Unarmed);
}

#[test]
fn queued_drafts_closed_and_unreadable_pull_requests_are_held() {
    let mut facts = armed();
    facts.queue = CarrierQueueFact::Queued;
    assert_eq!(held(&plan(&facts)), &CarrierHold::Queued);
    facts.queue = CarrierQueueFact::NotOpen;
    assert_eq!(held(&plan(&facts)), &CarrierHold::NotOpen);
    facts.queue = CarrierQueueFact::Unknown {
        detail: "timeline missing".to_owned(),
    };
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::QueueStateUnknown { .. }
    ));
    let mut facts = armed();
    facts.draft = true;
    assert_eq!(held(&plan(&facts)), &CarrierHold::Draft);
}

#[test]
fn a_removal_whose_required_job_failed_is_not_an_interruption() {
    // A failing job with an empty runner name still ran somewhere.
    for runner in [Some("m5-pulp-gate-01"), None, Some("")] {
        let facts = ejected_with(vec![job("macos", "failure", runner)]);
        assert_eq!(
            held(&plan(&facts)),
            &CarrierHold::RemovalNotInterruption {
                causes: vec!["gate_failed".to_owned()]
            },
            "{runner:?}"
        );
    }
}

#[test]
fn a_cancellation_after_a_runner_took_the_job_is_not_starvation() {
    let facts = ejected_with(vec![job("macos", "cancelled", Some("m1-pulp-gate-02"))]);
    assert_eq!(
        held(&plan(&facts)),
        &CarrierHold::RemovalNotInterruption {
            causes: vec!["cancelled_after_start".to_owned()]
        }
    );
}

#[test]
fn a_no_runner_cancel_within_seconds_is_not_starvation() {
    // A superseding push or concurrency cancel also leaves runner_name empty.
    let mut quick = job("macos", "cancelled", None);
    quick.completed_at = Some(at("2026-10-09T08:02:20Z"));
    let facts = ejected_with(vec![quick.clone()]);
    assert_eq!(
        held(&plan(&facts)),
        &CarrierHold::RemovalNotInterruption {
            causes: vec!["cancelled_before_wait".to_owned()]
        }
    );
    quick.created_at = None;
    let facts = ejected_with(vec![quick]);
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::RemovalNotInterruption { .. }
    ));
}

#[test]
fn conflict_and_manual_removals_are_left_to_people() {
    let mut facts = ejected_with(starved_macos());
    facts.queue = CarrierQueueFact::Ejected {
        reason: "merge_conflict".to_owned(),
        new_head_since: false,
        merge_group_commit: None,
    };
    assert_eq!(held(&plan(&facts)), &CarrierHold::RemovedForConflict);
    for reason in ["manual", "invalid_merge_commit", "branch_protection"] {
        facts.queue = CarrierQueueFact::Ejected {
            reason: reason.to_owned(),
            new_head_since: false,
            merge_group_commit: Some(GROUP.to_owned()),
        };
        assert!(
            matches!(held(&plan(&facts)), CarrierHold::RemovedByPerson { .. }),
            "{reason}"
        );
    }
}

#[test]
fn a_removal_with_no_attributable_job_is_held() {
    let mut facts = ejected_with(Vec::new());
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::RemovalUnclassified { .. }
    ));
    facts.runs.clear();
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::RemovalUnclassified { .. }
    ));
    let facts = ejected_with(vec![job("macos", "success", Some("m5"))]);
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::RemovalUnclassified { .. }
    ));
}

#[test]
fn a_removal_whose_group_run_is_still_live_is_held() {
    let mut facts = ejected_with(starved_macos());
    facts.runs[0].status = "in_progress".to_owned();
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::RemovalUnclassified { .. }
    ));
}

// True positives.

#[test]
fn a_starved_removal_rearms_the_exact_head() {
    let plan = plan(&ejected_with(starved_macos()));
    assert_eq!(
        plan.action(),
        Some(&CarrierAction::Rearm {
            head: HEAD.to_owned()
        })
    );
}

#[test]
fn an_advisory_failure_beside_a_starved_required_job_still_rearms() {
    let facts = ejected_with(vec![
        job("Linux (x64) [github-hosted]", "failure", Some("hosted")),
        job("macos", "cancelled", None),
    ]);
    assert!(matches!(
        plan(&facts).action(),
        Some(CarrierAction::Rearm { .. })
    ));
}

#[test]
fn a_starved_preamble_with_a_skipped_required_job_rearms() {
    let facts = ejected_with(vec![
        job("classify", "cancelled", None),
        job("macos", "skipped", None),
    ]);
    assert!(matches!(
        plan(&facts).action(),
        Some(CarrierAction::Rearm { .. })
    ));
}

#[test]
fn a_starved_removal_whose_head_checks_failed_since_is_not_rearmed() {
    let mut facts = ejected_with(starved_macos());
    facts.required = vec![required("macos", "FAILURE", 3)];
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::RequiredFailed { .. }
    ));
}

#[test]
fn a_cancelled_required_run_is_redispatched() {
    let mut facts = armed();
    facts.required = vec![required("macos", "CANCELLED", 9), green("Enforce")];
    facts.runs = vec![run(9, "pull_request", HEAD, "completed", 1)];
    assert_eq!(
        plan(&facts).action(),
        Some(&CarrierAction::Redispatch {
            head: HEAD.to_owned(),
            run_ids: vec![9]
        })
    );
}

#[test]
fn the_rerun_budget_is_read_from_github_attempts() {
    let mut facts = armed();
    facts.required = vec![required("macos", "CANCELLED", 9)];
    facts.runs = vec![run(9, "pull_request", HEAD, "completed", 2)];
    facts.runs[0].run_started_at = Some(at("2026-10-09T07:00:00Z"));
    assert!(plan(&facts).action().is_some(), "one rerun spent, one left");
    facts.runs[0].run_attempt = 3;
    assert_eq!(
        held(&plan(&facts)),
        &CarrierHold::RedispatchBudgetSpent { run_ids: vec![9] }
    );
}

#[test]
fn reruns_are_bounded_per_pull_request_per_hour() {
    let mut facts = armed();
    facts.required = vec![
        required("macos", "CANCELLED", 9),
        required("Enforce", "CANCELLED", 10),
    ];
    let mut recent_a = run(9, "pull_request", HEAD, "completed", 2);
    recent_a.run_started_at = Some(at("2026-10-09T09:30:00Z"));
    let mut other = run(10, "pull_request", HEAD, "completed", 1);
    other.workflow = "version-skill-check.yml".to_owned();
    facts.runs = vec![recent_a.clone(), other.clone()];
    // One rerun inside the hour leaves room for one more.
    let plan_one = plan(&facts);
    assert!(
        matches!(plan_one.action(), Some(CarrierAction::Redispatch { run_ids, .. }) if run_ids.len() == 1),
        "{plan_one:?}"
    );
    let mut recent_b = run(11, "pull_request", HEAD, "completed", 2);
    recent_b.workflow = "drift-fast.yml".to_owned();
    recent_b.conclusion = Some("success".to_owned());
    recent_b.run_started_at = Some(at("2026-10-09T09:45:00Z"));
    facts.runs = vec![recent_a, other, recent_b];
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::RedispatchBudgetSpent { .. }
    ));
}

#[test]
fn a_live_retry_of_the_same_workflow_holds_the_redispatch() {
    let mut facts = armed();
    facts.required = vec![required("macos", "CANCELLED", 9)];
    facts.runs = vec![
        run(9, "pull_request", HEAD, "completed", 1),
        run(12, "pull_request", HEAD, "in_progress", 1),
    ];
    assert_eq!(
        held(&plan(&facts)),
        &CarrierHold::RunStillLive { run_ids: vec![12] }
    );
}

#[test]
fn a_cancelled_status_with_no_readable_run_is_held() {
    let mut facts = armed();
    facts.required = vec![RequiredFact {
        context: "Vellum freeze".to_owned(),
        status: "COMPLETED".to_owned(),
        conclusion: Some("CANCELLED".to_owned()),
        run_id: None,
    }];
    assert_eq!(
        held(&plan(&facts)),
        &CarrierHold::RunFactMissing {
            contexts: vec!["Vellum freeze".to_owned()]
        }
    );
    facts.required[0].run_id = Some(9);
    facts.runs = vec![run(9, "pull_request", GROUP, "completed", 1)];
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::RunFactMissing { .. }
    ));
}

#[test]
fn a_green_behind_head_proposes_a_merge_update_and_a_clean_one_waits() {
    let mut facts = armed();
    facts.merge_state = "BEHIND".to_owned();
    assert_eq!(
        plan(&facts).action(),
        Some(&CarrierAction::UpdateBranch {
            head: HEAD.to_owned()
        })
    );
    facts.merge_state = "CLEAN".to_owned();
    assert_eq!(held(&plan(&facts)), &CarrierHold::AwaitingQueue);
    facts.required = vec![RequiredFact {
        context: "macos".to_owned(),
        status: "IN_PROGRESS".to_owned(),
        conclusion: None,
        run_id: Some(4),
    }];
    facts.merge_state = "BEHIND".to_owned();
    assert!(matches!(
        held(&plan(&facts)),
        CarrierHold::WaitingRequired { .. }
    ));
}

#[test]
fn facts_and_plans_round_trip_as_json_for_replay() {
    let facts = ejected_with(starved_macos());
    let encoded = serde_json::to_value(&facts).expect("encode facts");
    let decoded: CarrierFacts = serde_json::from_value(encoded).expect("decode facts");
    assert_eq!(decoded, facts);
    let plan = plan(&decoded);
    assert_eq!(
        serde_json::to_value(&plan).expect("encode plan"),
        json!({"number": 7, "head_sha": HEAD, "decision": "propose", "action": "rearm", "head": HEAD})
    );
    let hold = super::plan(&armed());
    assert_eq!(
        serde_json::to_value(&hold).expect("encode hold"),
        json!({"number": 7, "head_sha": HEAD, "decision": "hold", "hold": "awaiting_queue"})
    );
}

#[test]
fn class_names_parse_both_spellings() {
    assert_eq!(
        CarrierClass::parse("redispatch"),
        Some(CarrierClass::Redispatch)
    );
    assert_eq!(CarrierClass::parse("rearm"), Some(CarrierClass::Rearm));
    assert_eq!(
        CarrierClass::parse("update-branch"),
        Some(CarrierClass::UpdateBranch)
    );
    assert_eq!(CarrierClass::parse("merge"), None);
}
