//! Tests for the fleet slot assertions.
//!
//! The lane names, counts and log lines below are the ones measured on the host
//! these assertions were written against, so each fixture reproduces the shape
//! of the stall rather than an idealisation of it.
//!
//! Every detector here ships a **planted negative control that must go red**:
//! the coherent fixture next to the incident one, the correct refusal next to
//! the defective reservation. A control is only worth its line count if it can
//! fail, so each is written to break when the corresponding rule is loosened —
//! the incident test alone could be firing on anything.

use chrono::{Duration, TimeZone, Utc};

use super::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 4, 23, 44, 0).unwrap()
}

const RELEASE_LINE: &str = "release:         yielding 20s (queued=2 priority_demand=1 \
                            running_macos_vms=0/2) — priority lane 'Build and Test' has the slot";
const GATE_LINE: &str = "pulp-gate:       waiting 20s (queued=0 running_macos_vms=0/2 \
                         priority_demand=0)";
const GATE_SLOT2_LINE: &str = "pulp-gate.slot2: waiting 20s (queued=0 running_macos_vms=0/2 \
                               priority_demand=0)";

fn release_ctx() -> SupervisorContext<'static> {
    SupervisorContext {
        host: "m5",
        repo: "Generous-Corp/pulp",
        priority_lane: None,
    }
}

fn gate_ctx() -> SupervisorContext<'static> {
    SupervisorContext {
        host: "m5",
        repo: "Generous-Corp/pulp",
        priority_lane: Some("Build and Test"),
    }
}

fn parsed(ctx: SupervisorContext<'_>, line: &str) -> SupervisorObservation {
    parse_supervisor_line(ctx, line).expect("fixture line must parse")
}

/// The measured incident: release yields, both gate supervisors report nothing.
fn incident_observations() -> Vec<SupervisorObservation> {
    vec![
        parsed(release_ctx(), RELEASE_LINE),
        parsed(gate_ctx(), GATE_LINE),
        parsed(gate_ctx(), GATE_SLOT2_LINE),
    ]
}

fn evidence_with(jobs: Vec<PriorityJob>, own_demand_minutes: &[i64]) -> PriorityDemandEvidence {
    PriorityDemandEvidence {
        scan_boundary: None,
        counted_jobs: jobs,
        own_queued_since: own_demand_minutes
            .iter()
            .map(|minutes| now() - Duration::minutes(*minutes))
            .collect(),
    }
}

fn job(name: &str, state: PriorityJobState, routes_self_hosted: bool) -> PriorityJob {
    PriorityJob {
        name: name.to_owned(),
        state,
        routes_self_hosted,
    }
}

fn assess(
    observation: &SupervisorObservation,
    evidence: &PriorityDemandEvidence,
) -> SlotWithholdReport {
    assess_slot_withholding(
        observation,
        evidence,
        SlotWithholdThresholds::default(),
        now(),
    )
}

// ---------------------------------------------------------------------------
// Parsing the two live log forms
// ---------------------------------------------------------------------------

#[test]
fn parses_the_yielding_form_with_its_priority_lane_citation() {
    let observation = parsed(release_ctx(), RELEASE_LINE);
    assert_eq!(observation.lane, "release");
    assert_eq!(observation.host, "m5");
    assert_eq!(observation.repo, "Generous-Corp/pulp");
    assert_eq!(observation.queued, 2);
    assert_eq!(observation.priority_demand, 1);
    assert_eq!(observation.running_vms, 0);
    assert_eq!(observation.capacity, 2);
    assert_eq!(observation.free_slots(), 2);
    assert!(observation.yielded());
    assert_eq!(
        observation.yield_state,
        YieldState::ForPriorityLane {
            lane: "Build and Test".to_owned(),
        }
    );
}

#[test]
fn parses_the_waiting_form_with_fields_in_a_different_order() {
    // The waiting form prints `running_macos_vms` before `priority_demand`; a
    // positional parser reads this line wrong while appearing to succeed.
    let observation = parsed(gate_ctx(), GATE_SLOT2_LINE);
    assert_eq!(observation.lane, "pulp-gate.slot2");
    assert_eq!(observation.queued, 0);
    assert_eq!(observation.priority_demand, 0);
    assert_eq!(observation.free_slots(), 2);
    assert!(!observation.yielded());
    assert_eq!(observation.yield_state, YieldState::Waiting);
    assert!(observation.serves_priority_lane("build and test"));
}

#[test]
fn parses_a_host_health_yield_as_its_own_reason() {
    let line = "release: yielding 20s (queued=3 priority_demand=0 running_macos_vms=0/2 \
                host_health_yield=1)";
    assert_eq!(
        parsed(release_ctx(), line).yield_state,
        YieldState::ForHostHealth
    );
}

#[test]
fn ignores_unknown_fields_but_rejects_a_malformed_known_one() {
    let extra = "release: waiting 20s (queued=1 priority_demand=0 running_macos_vms=1/2 \
                 lease_age=44s)";
    assert_eq!(parsed(release_ctx(), extra).queued, 1);

    let malformed = "release: waiting 20s (queued=many priority_demand=0 running_macos_vms=1/2)";
    let error = parse_supervisor_line(release_ctx(), malformed).expect_err("must not default");
    assert_eq!(error.boundary, Boundary::Parse);
    assert!(error.detail.contains("queued=many"), "{}", error.detail);
}

// ---------------------------------------------------------------------------
// Cross-supervisor coherence — the incident, and its control
// ---------------------------------------------------------------------------

#[test]
fn incident_release_yielding_while_both_gates_report_zero_raises() {
    let report = assess_supervisor_coherence(&incident_observations());

    assert!(report.verdict.is_raise(), "{report:?}");
    assert_eq!(report.verdict, ServiceVerdict::Starved);
    assert_eq!(report.contradictions.len(), 1);

    let found = &report.contradictions[0];
    assert_eq!(found.citing_lane, "release");
    assert_eq!(found.cited_lane, "Build and Test");
    assert_eq!(
        found.corroborating_lanes,
        vec!["pulp-gate".to_owned(), "pulp-gate.slot2".to_owned()]
    );
    assert_eq!(found.free_slots, 2);

    // The detail must name the contradiction and the specific lane pair, not
    // merely report a number: an operator reading it should not have to re-open
    // the logs to know which two supervisors disagree.
    assert!(
        found.detail.contains("`release` yielded citing"),
        "{}",
        found.detail
    );
    assert!(
        found.detail.contains("'Build and Test'"),
        "{}",
        found.detail
    );
    assert!(found.detail.contains("pulp-gate.slot2"), "{}", found.detail);
    assert!(found.detail.contains("queued=0"), "{}", found.detail);
    assert!(
        found.detail.contains("cannot both be right"),
        "{}",
        found.detail
    );
}

/// CONTROL for the test above. Same three supervisors, same citation, one fact
/// changed: the cited lane genuinely has work queued. If this raises, the
/// incident test is firing on the fixture's shape rather than on the
/// disagreement.
#[test]
fn control_a_cited_lane_with_real_queued_work_does_not_raise() {
    let mut observations = incident_observations();
    observations[1].queued = 1;

    let report = assess_supervisor_coherence(&observations);

    assert!(!report.verdict.is_raise(), "{report:?}");
    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert!(report.contradictions.is_empty());
    assert!(report.uncorroborated.is_empty());
}

#[test]
fn differing_queued_counts_between_unrelated_lanes_are_not_contradictions() {
    // Lanes watch different label sets, so a naive "all numbers must match"
    // check would fire here. It must not.
    let mut observations = incident_observations();
    observations[0].yield_state = YieldState::Waiting;
    observations[0].queued = 7;
    observations[1].queued = 0;
    observations[2].queued = 3;

    let report = assess_supervisor_coherence(&observations);
    assert!(!report.verdict.is_raise(), "{report:?}");
    assert!(report.contradictions.is_empty());
}

#[test]
fn a_citation_no_observed_supervisor_serves_is_unknown_not_a_pass() {
    let observations = vec![
        parsed(release_ctx(), RELEASE_LINE),
        parsed(release_ctx(), GATE_LINE),
    ];
    let report = assess_supervisor_coherence(&observations);

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Scope));
    assert_eq!(report.uncorroborated.len(), 1);
}

#[test]
fn a_lone_supervisor_cannot_contradict_itself_and_abstains() {
    let report = assess_supervisor_coherence(&[parsed(release_ctx(), RELEASE_LINE)]);
    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Scope));
    assert!(report.detail.contains("cannot contradict itself"));
}

#[test]
fn supervisors_on_a_different_host_are_never_compared() {
    let mut observations = incident_observations();
    observations[1].host = "m3".to_owned();
    observations[2].host = "m3".to_owned();

    let report = assess_supervisor_coherence(&observations);
    // Nothing on m5 serves the cited lane, so the check abstains rather than
    // borrowing another host's readings as an oracle.
    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Scope));
}

// ---------------------------------------------------------------------------
// Withheld capacity — the four causes, each distinguishable
// ---------------------------------------------------------------------------

#[test]
fn cause_one_genuinely_queued_priority_demand_is_correct_behaviour() {
    let observation = parsed(release_ctx(), RELEASE_LINE);
    let evidence = evidence_with(
        vec![job("macos", PriorityJobState::Queued, true)],
        &[30, 20],
    );

    let report = assess(&observation, &evidence);

    assert_eq!(report.cause, WithholdCause::QueuedPriorityDemand);
    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert!(!report.verdict.is_raise());
    assert!(report.cause.is_correct_behaviour());
    assert_eq!(report.oldest_demand_secs, Some(1800));
    assert!(
        report.detail.contains("genuinely queued"),
        "{}",
        report.detail
    );
    assert!(report.detail.contains("Wait."), "{}", report.detail);
}

#[test]
fn cause_two_an_in_progress_job_that_cannot_take_the_slot_raises() {
    let observation = parsed(release_ctx(), RELEASE_LINE);
    let evidence = evidence_with(
        vec![job("resolve-runs-on", PriorityJobState::InProgress, false)],
        &[30, 20],
    );

    let report = assess(&observation, &evidence);

    assert_eq!(report.cause, WithholdCause::UnusablePriorityJob);
    assert_eq!(report.verdict, ServiceVerdict::Degraded);
    assert!(report.verdict.is_raise());
    assert!(!report.cause.is_correct_behaviour());
    assert!(report.boundary.is_none());
    assert!(
        report
            .detail
            .contains("no job that can occupy a self-hosted slot"),
        "{}",
        report.detail
    );
    assert!(
        report
            .detail
            .contains("`resolve-runs-on` is in_progress and hosted-only"),
        "{}",
        report.detail
    );
    assert!(
        report
            .detail
            .contains("Stop counting jobs that cannot take the slot"),
        "{}",
        report.detail
    );
}

#[test]
fn cause_two_also_covers_a_count_with_no_job_behind_it() {
    let observation = parsed(release_ctx(), RELEASE_LINE);
    let report = assess(&observation, &evidence_with(Vec::new(), &[30]));

    assert_eq!(report.cause, WithholdCause::UnusablePriorityJob);
    assert!(
        report.detail.contains("counted no job at all"),
        "{}",
        report.detail
    );
}

#[test]
fn cause_three_a_fail_closed_scan_raises_and_names_the_scan() {
    let observation = parsed(release_ctx(), RELEASE_LINE);
    let evidence = PriorityDemandEvidence {
        scan_boundary: Some(Boundary::Transport),
        counted_jobs: Vec::new(),
        own_queued_since: vec![now() - Duration::minutes(30)],
    };

    let report = assess(&observation, &evidence);

    assert_eq!(report.cause, WithholdCause::FailClosedScan);
    assert_eq!(report.verdict, ServiceVerdict::Degraded);
    assert!(report.verdict.is_raise());
    assert!(
        report
            .detail
            .contains("priority-demand scan failed (transport)"),
        "{}",
        report.detail
    );
    assert!(report.detail.contains("fix the scan"), "{}", report.detail);
    // The transport boundary's own advice must ride along: re-authenticating in
    // response to a timeout is the mistake this phrasing exists to prevent.
    assert!(
        report
            .detail
            .contains("This is not an authentication fault"),
        "{}",
        report.detail
    );
}

/// The distinction between causes 1, 2 and 3 is the entire product. If two of
/// these details ever read alike, the operator is sent to the wrong subsystem.
#[test]
fn the_three_citation_causes_have_distinguishable_details() {
    let observation = parsed(release_ctx(), RELEASE_LINE);
    let cause_one = assess(
        &observation,
        &evidence_with(vec![job("macos", PriorityJobState::Queued, true)], &[30]),
    );
    let cause_two = assess(
        &observation,
        &evidence_with(
            vec![job("resolve-runs-on", PriorityJobState::InProgress, false)],
            &[30],
        ),
    );
    let cause_three = assess(
        &observation,
        &PriorityDemandEvidence {
            scan_boundary: Some(Boundary::Transport),
            counted_jobs: Vec::new(),
            own_queued_since: vec![now() - Duration::minutes(30)],
        },
    );

    let details = [
        cause_one.detail.as_str(),
        cause_two.detail.as_str(),
        cause_three.detail.as_str(),
    ];
    for (index, left) in details.iter().enumerate() {
        for right in details.iter().skip(index + 1) {
            assert_ne!(left, right, "two causes print the same line");
        }
    }

    // Each remedy appears in exactly one of the three.
    for phrase in [
        "Wait.",
        "Stop counting jobs that cannot take the slot",
        "fix the scan",
    ] {
        let hits = details.iter().filter(|text| text.contains(phrase)).count();
        assert_eq!(hits, 1, "`{phrase}` must identify exactly one cause");
    }

    assert_ne!(cause_one.verdict, cause_two.verdict);
    assert_eq!(cause_two.verdict, cause_three.verdict);
    assert_ne!(cause_two.cause, cause_three.cause);
}

/// CONTROL. Free slots, aged demand, a supervisor declining — the exact input
/// shape of the defect — but the refusal is the host-health gate protecting a
/// saturated machine. Reporting this as a defect would send an operator to boot
/// a VM into an out-of-memory host. If this raises, the detector is keying on
/// "yielded with free slots" instead of on the reason.
#[test]
fn control_a_host_health_refusal_with_free_slots_and_aged_demand_is_not_a_defect() {
    let line = "release: yielding 20s (queued=2 priority_demand=0 running_macos_vms=0/2 \
                host_health_yield=1)";
    let observation = parsed(release_ctx(), line);
    let report = assess(&observation, &evidence_with(Vec::new(), &[45]));

    assert_eq!(report.free_slots, 2);
    assert_eq!(report.oldest_demand_secs, Some(2700));
    assert_eq!(report.cause, WithholdCause::HostHealthSaturation);
    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert!(!report.verdict.is_raise(), "{report:?}");
    assert!(report.cause.is_correct_behaviour());
    assert!(
        report.detail.contains("correct refusal, not a slot defect"),
        "{}",
        report.detail
    );
    assert!(
        report.detail.contains("Free memory on m5"),
        "{}",
        report.detail
    );
}

/// CONTROL. A yield inside the transient window is how a just-in-time pool
/// normally behaves; judging it would make the assertion fire every boot.
#[test]
fn control_a_yield_with_demand_inside_the_transient_window_is_not_a_defect() {
    let observation = parsed(release_ctx(), RELEASE_LINE);
    let report = assess(&observation, &evidence_with(Vec::new(), &[2]));

    assert_eq!(report.cause, WithholdCause::None);
    assert!(!report.verdict.is_raise());
    assert!(
        report.detail.contains("inside the 300s transient window"),
        "{}",
        report.detail
    );
}

/// CONTROL. With every slot occupied there is no capacity to withhold, however
/// bogus the count behind the citation is.
#[test]
fn control_a_yield_with_no_free_slot_withholds_nothing() {
    let line = "release: yielding 20s (queued=2 priority_demand=1 running_macos_vms=2/2) \
                — priority lane 'Build and Test' has the slot";
    let observation = parsed(release_ctx(), line);
    let report = assess(&observation, &evidence_with(Vec::new(), &[45]));

    assert_eq!(report.cause, WithholdCause::None);
    assert!(!report.verdict.is_raise());
    assert!(
        report.detail.contains("no free slot to withhold"),
        "{}",
        report.detail
    );
}

#[test]
fn a_waiting_supervisor_is_not_withholding_anything() {
    let observation = parsed(gate_ctx(), GATE_LINE);
    let report = assess(&observation, &evidence_with(Vec::new(), &[45]));

    assert_eq!(report.cause, WithholdCause::None);
    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert!(report.detail.contains("ready to take work"));
}

// ---------------------------------------------------------------------------
// Unreadable instruments are never a pass
// ---------------------------------------------------------------------------

#[test]
fn an_unparseable_supervisor_line_is_unknown_with_a_named_boundary() {
    let error = parse_supervisor_line(release_ctx(), "release supervisor tick ok")
        .expect_err("garbage must not parse");
    assert_eq!(error.boundary, Boundary::Parse);

    let report = error.into_report("m5", "Generous-Corp/pulp", "release");
    assert_eq!(report.cause, WithholdCause::Unreadable);
    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert!(report.verdict.is_raise());
    assert_eq!(report.boundary, Some(Boundary::Parse));
    assert!(report.detail.contains("no slot claim is made"));
}

#[test]
fn an_absent_supervisor_log_is_unknown_with_a_transport_boundary() {
    let error = SupervisorLogError {
        boundary: Boundary::Transport,
        detail: "no supervisor log on m5 for lane `release`".to_owned(),
        raw: String::new(),
    };
    let report = error.into_report("m5", "Generous-Corp/pulp", "release");

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert!(report.verdict.is_raise());
    assert_eq!(report.boundary, Some(Boundary::Transport));
    assert!(report.detail.contains("no supervisor log on m5"));
}

#[test]
fn a_yield_with_no_stated_reason_is_unknown_rather_than_tolerated() {
    let line = "release: yielding 20s (queued=2 priority_demand=1 running_macos_vms=0/2)";
    let observation = parsed(release_ctx(), line);
    let report = assess(&observation, &evidence_with(Vec::new(), &[45]));

    assert_eq!(report.cause, WithholdCause::Unreadable);
    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Parse));
    assert!(report.detail.contains("without naming a reason"));
}

#[test]
fn every_unknown_verdict_carries_a_boundary_and_no_other_verdict_does() {
    let observation = parsed(release_ctx(), RELEASE_LINE);
    let reports = [
        assess(
            &observation,
            &evidence_with(vec![job("macos", PriorityJobState::Queued, true)], &[30]),
        ),
        assess(&observation, &evidence_with(Vec::new(), &[30])),
        assess(&observation, &evidence_with(Vec::new(), &[1])),
    ];
    for report in &reports {
        assert_eq!(
            report.verdict == ServiceVerdict::Unknown,
            report.boundary.is_some(),
            "{report:?}"
        );
    }
}
