//! Tests for the fleet relay hop assertions.
//!
//! The fixtures are the measured numbers, not idealisations of them: the
//! incident's dead first hop cost 18s before the fallback answered in 0.27s,
//! and the healthy fleet reading taken afterwards was 0.16s / 0.27s / 0.19s
//! against a 5s budget.
//!
//! Every check ships a **planted negative control that must go red**, because
//! an assertion that cannot fail its own test is precisely what this module
//! exists to replace:
//!
//! * a wholly healthy relay is asserted to be `Served` and not to raise, so
//!   `Degraded` is not the only value the assertion can produce;
//! * the naive question — "did any hop connect?" — is asserted to *pass* on the
//!   incident fixture while the verdict is still `Degraded`. That pairing is
//!   the whole slice: the relay answered throughout the outage;
//! * one hop's measurement is asserted to flip verdict on the budget alone;
//! * the same broken hop is asserted to produce different severity first versus
//!   last, so position is proven to be read rather than merely stored;
//! * an unmeasured hop is asserted not to be a pass even when every other hop
//!   is healthy — and the same fixture with the hop measured is asserted to
//!   stop being `Unknown`, so the boundary is not simply always emitted.

use chrono::{TimeZone, Utc};

use super::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 4, 23, 44, 0).unwrap()
}

fn assess(probes: &[HopProbe]) -> RelayReport {
    assess_relay(
        "http_connect_ssh_relay",
        probes,
        RelayThresholds::default(),
        now(),
    )
}

fn assess_with_budget(probes: &[HopProbe], hop_budget_secs: f64) -> RelayReport {
    assess_relay(
        "http_connect_ssh_relay",
        probes,
        RelayThresholds { hop_budget_secs },
        now(),
    )
}

/// The declared order that produced the incident: `--relay-host macmini
/// --relay-host m1`, with macmini unreachable and m1 answering fine.
fn incident_probes() -> Vec<HopProbe> {
    vec![
        HopProbe::timed_out("macmini", 18.0),
        HopProbe::connected("m1", 0.27),
    ]
}

/// The same relay measured healthy afterwards, hop times as read off the fleet.
fn healthy_probes() -> Vec<HopProbe> {
    vec![
        HopProbe::connected("macmini", 0.16),
        HopProbe::connected("m1", 0.27),
    ]
}

fn close(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-9
}

// ---------------------------------------------------------------------------
// The incident.
// ---------------------------------------------------------------------------

#[test]
fn dead_first_hop_degrades_the_relay_even_though_the_fallback_succeeds() {
    let report = assess(&incident_probes());

    assert_eq!(report.verdict, ServiceVerdict::Degraded);
    assert!(report.verdict.is_raise());
    assert_eq!(report.first_answering_position, Some(2));
    assert!(close(report.tax_secs.expect("a hop answered"), 18.0));

    let detail = report.detail.to_ascii_lowercase();
    assert!(
        detail.contains("every connection pays"),
        "the tax must be stated as a per-connection cost: {}",
        report.detail
    );
    assert!(
        detail.contains("18.00s of tax"),
        "the tax must be quantified: {}",
        report.detail
    );
    assert!(
        detail.contains("answers") && detail.contains("fallback succeeds"),
        "the detail must say the relay answered anyway — that is why it was missed: {}",
        report.detail
    );
}

/// **Control.** Without this, "the relay is degraded" could be the only thing
/// the assertion is able to say.
#[test]
fn control_both_hops_healthy_serves_and_does_not_raise() {
    let report = assess(&healthy_probes());

    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert!(!report.verdict.is_raise());
    assert!(report.boundary.is_none());
    assert!(close(report.tax_secs.expect("a hop answered"), 0.0));
    assert!(report.taxing_hops().is_empty());
    assert!(
        report
            .hops
            .iter()
            .all(|hop| hop.verdict == ServiceVerdict::Served),
        "every hop connected inside budget: {report:?}"
    );
}

/// **Control — the sharp one.** An assertion that only asked "did any hop
/// connect?" passes the incident fixture, because the fallback answered. That
/// question was GREEN for the whole outage. Proving both halves in one test is
/// the reason this module asserts per declared hop instead.
#[test]
fn control_naive_any_hop_connected_passes_the_incident_that_still_degrades() {
    let report = assess(&incident_probes());

    assert!(
        report.any_hop_connected(),
        "the incident fixture MUST have a reachable hop — otherwise this control proves nothing"
    );
    assert_eq!(
        report.verdict,
        ServiceVerdict::Degraded,
        "a reachable hop is not a healthy relay"
    );
    assert!(report.verdict.is_raise());

    // The same naive fact is true of the healthy relay, so on its own it
    // separates nothing at all.
    let healthy = assess(&healthy_probes());
    assert!(healthy.any_hop_connected());
    assert_eq!(
        healthy.any_hop_connected(),
        report.any_hop_connected(),
        "the naive check cannot tell the outage and the healthy fleet apart; the verdict can"
    );
    assert_ne!(healthy.verdict, report.verdict);
}

// ---------------------------------------------------------------------------
// Budget: a ratio, not a boolean.
// ---------------------------------------------------------------------------

#[test]
fn hop_over_budget_degrades_and_reports_the_ratio() {
    let probes = vec![HopProbe::connected("m5-via-proxy", 5.5)];
    let report = assess_with_budget(&probes, 5.0);

    assert_eq!(report.verdict, ServiceVerdict::Degraded);
    let hop = &report.hops[0];
    assert_eq!(hop.outcome, HopOutcome::OverBudget);
    assert!(close(hop.budget_ratio.expect("measured"), 1.1));
    assert!(
        hop.detail.contains("110% of it"),
        "the ratio must be in the detail, not just the struct: {}",
        hop.detail
    );
}

/// **Control.** Identical measurement, larger budget: same shape, opposite
/// verdict. Only the budget differs, so the verdict is proven to come from the
/// comparison rather than from the outcome variant.
#[test]
fn control_same_hop_under_a_larger_budget_serves() {
    let probes = vec![HopProbe::connected("m5-via-proxy", 5.5)];
    let report = assess_with_budget(&probes, 10.0);

    assert_eq!(report.verdict, ServiceVerdict::Served);
    let hop = &report.hops[0];
    assert_eq!(hop.outcome, HopOutcome::WithinBudget);
    assert!(close(hop.budget_ratio.expect("measured"), 0.55));
    assert!(
        hop.detail.contains("55% of it"),
        "a passing hop still reports its ratio — 55% and 4% are not the same lane: {}",
        hop.detail
    );
}

/// A hop at 98% of its budget passes, and must still be visibly distinct from
/// one at 4%. Collapsing both to "connected" is how the margin disappears.
#[test]
fn a_hop_near_its_ceiling_passes_but_reports_a_near_ceiling_ratio() {
    let comfortable = assess_with_budget(&[HopProbe::connected("m1", 0.2)], 5.0);
    let marginal = assess_with_budget(&[HopProbe::connected("m1", 4.9)], 5.0);

    assert_eq!(comfortable.verdict, ServiceVerdict::Served);
    assert_eq!(marginal.verdict, ServiceVerdict::Served);
    assert!(close(
        comfortable.hops[0].budget_ratio.expect("measured"),
        0.04
    ));
    assert!(close(
        marginal.hops[0].budget_ratio.expect("measured"),
        0.98
    ));
    assert_ne!(comfortable.hops[0].detail, marginal.hops[0].detail);
}

// ---------------------------------------------------------------------------
// Position: the same defect costs differently depending on where it sits.
// ---------------------------------------------------------------------------

#[test]
fn the_same_dead_hop_is_worse_first_than_last() {
    let first = assess(&[
        HopProbe::timed_out("macmini", 18.0),
        HopProbe::connected("m1", 0.27),
    ]);
    let last = assess(&[
        HopProbe::connected("m1", 0.27),
        HopProbe::timed_out("macmini", 18.0),
    ]);

    // Severity differs.
    assert_eq!(first.verdict, ServiceVerdict::Degraded);
    assert_eq!(last.verdict, ServiceVerdict::Idle);
    assert!(
        first.verdict > last.verdict,
        "first-position failure is worse"
    );
    assert!(first.verdict.is_raise());
    assert!(!last.verdict.is_raise());

    // And so does the per-hop reading of the identical host.
    let dead_first = &first.hops[0];
    let dead_last = &last.hops[1];
    assert_eq!(dead_first.host, dead_last.host);
    assert_eq!(dead_first.outcome, dead_last.outcome);
    assert!(dead_first.attempted);
    assert!(!dead_last.attempted);
    assert_eq!(dead_first.verdict, ServiceVerdict::Degraded);
    assert_eq!(dead_last.verdict, ServiceVerdict::Idle);

    // And the cost differs, which is the reason the severity does.
    assert!(close(first.tax_secs.expect("answered"), 18.0));
    assert!(close(last.tax_secs.expect("answered"), 0.0));
    assert!(
        last.detail.contains("never dialled"),
        "a shadowed dead hop costs nothing today, and the message must say so: {}",
        last.detail
    );
    assert!(
        last.detail.contains("fallback"),
        "what a shadowed dead hop loses is the fallback: {}",
        last.detail
    );
}

/// Everything up to the first answering hop is dialled, so "not first" does not
/// mean "free" — only "behind an answer" does.
#[test]
fn a_dead_hop_is_free_only_once_something_ahead_of_it_answers() {
    let shadowed = assess(&[
        HopProbe::connected("macmini", 0.16),
        HopProbe::timed_out("m1", 18.0),
        HopProbe::connected("m3", 0.19),
    ]);

    assert_eq!(shadowed.first_answering_position, Some(1));
    assert!(!shadowed.hops[1].attempted);
    assert_eq!(shadowed.verdict, ServiceVerdict::Idle);
    assert!(close(shadowed.tax_secs.expect("answered"), 0.0));

    // Move the same dead hop ahead of every answer and it becomes a tax again.
    let leading = assess(&[
        HopProbe::timed_out("m1", 18.0),
        HopProbe::connected("macmini", 0.16),
        HopProbe::connected("m3", 0.19),
    ]);
    assert!(leading.hops[0].attempted);
    assert_eq!(leading.verdict, ServiceVerdict::Degraded);
    assert!(close(leading.tax_secs.expect("answered"), 18.0));
}

// ---------------------------------------------------------------------------
// Unmeasurable is never a pass.
// ---------------------------------------------------------------------------

#[test]
fn an_unmeasured_hop_is_unknown_with_a_named_boundary() {
    let report = assess(&[
        HopProbe::unmeasured("macmini", Boundary::Transport),
        HopProbe::connected("m1", 0.27),
    ]);

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Transport));
    assert!(report.verdict.is_raise());
    assert!(
        report.detail.contains("no claim is made"),
        "an unmeasured hop must withhold the claim, not soften it: {}",
        report.detail
    );
    assert!(
        report.hops[0].remedy.contains("env -i"),
        "the remedy must name the control that stripped the proxy variables: {}",
        report.hops[0].remedy
    );
}

/// **Control.** The identical relay with the hop actually measured is not
/// `Unknown`, so the boundary is produced by the missing measurement rather
/// than emitted unconditionally.
#[test]
fn control_the_same_relay_with_the_hop_measured_is_not_unknown() {
    let report = assess(&healthy_probes());

    assert_ne!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, None);
    assert!(report.hops.iter().all(|hop| hop.boundary.is_none()));
}

#[test]
fn a_relay_declaring_no_hop_asserts_nothing() {
    let report = assess(&[]);

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Parse));
    assert!(!report.any_hop_connected());
}

#[test]
fn a_non_positive_budget_asserts_nothing() {
    let report = assess_with_budget(&healthy_probes(), 0.0);

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Parse));
}

#[test]
fn a_relay_no_hop_answers_is_unserved_not_degraded() {
    let report = assess(&[
        HopProbe::timed_out("macmini", 18.0),
        HopProbe::refused("m1", 0.05),
        HopProbe::unresolved("m3", 0.01),
    ]);

    assert_eq!(report.verdict, ServiceVerdict::Unserved);
    assert!(!report.any_hop_connected());
    assert_eq!(
        report.tax_secs, None,
        "nothing is a tax when nothing gets through"
    );
    assert!(report.detail.contains("severed"));
}

// ---------------------------------------------------------------------------
// The measured healthy fleet, as a positive-control fixture.
// ---------------------------------------------------------------------------

#[test]
fn the_measured_healthy_fleet_serves_on_every_hop() {
    let report = assess(&[
        HopProbe::connected("macmini", 0.16),
        HopProbe::connected("m1", 0.27),
        HopProbe::connected("m3", 0.19),
    ]);

    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert_eq!(report.first_answering_position, Some(1));
    assert!(
        report
            .hops
            .iter()
            .all(|hop| hop.outcome == HopOutcome::WithinBudget)
    );
    assert!(report.detail.contains("3 declared hop(s) connect inside"));
}

// ---------------------------------------------------------------------------
// The self-heal proposal: bounded, described, never applied.
// ---------------------------------------------------------------------------

#[test]
fn the_proposal_drops_the_dead_leader_and_puts_healthy_hops_first() {
    let report = assess(&[
        HopProbe::timed_out("macmini", 18.0),
        HopProbe::connected("m1", 0.27),
        HopProbe::connected("m3", 0.19),
    ]);
    let proposal = propose_hop_order(&report);

    assert_eq!(proposal.status, ProposalStatus::Proposed);
    assert!(proposal.is_change());
    assert_eq!(proposal.proposed_order, vec!["m1", "m3"]);
    assert_eq!(proposal.dropped, vec!["macmini"]);
    assert!(proposal.refused_drops.is_empty());
    assert_ne!(
        proposal.proposed_order.first().map(String::as_str),
        Some("macmini"),
        "the hop that taxed every connection must not remain the leader"
    );
    assert!(
        proposal.rationale.contains("18.00s of tax"),
        "the proposal must quantify what it removes: {}",
        proposal.rationale
    );
    assert!(
        proposal.rationale.contains("nothing is applied"),
        "this is a description of an edit, not an edit: {}",
        proposal.rationale
    );
}

/// Slow hops are ordered cheapest-first, so if the relay must run on an
/// over-budget hop it runs on the least expensive one available.
#[test]
fn the_proposal_orders_surviving_slow_hops_cheapest_first() {
    let report = assess(&[
        HopProbe::connected("macmini", 9.0),
        HopProbe::connected("m1", 6.0),
    ]);
    let proposal = propose_hop_order(&report);

    assert_eq!(proposal.status, ProposalStatus::Proposed);
    assert_eq!(proposal.proposed_order, vec!["m1", "macmini"]);
    assert!(proposal.dropped.is_empty());
}

/// **The refusal.** The surviving hop misses its budget, so it is a drop
/// candidate — and dropping it would sever the relay. A slow relay is
/// recoverable; a severed one is an outage.
#[test]
fn the_proposal_refuses_to_drop_the_last_hop_that_connects() {
    let report = assess(&[
        HopProbe::timed_out("macmini", 18.0),
        HopProbe::connected("m1", 6.5),
    ]);
    let proposal = propose_hop_order(&report);

    assert_eq!(
        proposal.dropped,
        vec!["macmini"],
        "the dead hop is still dropped"
    );
    assert_eq!(
        proposal.refused_drops,
        vec!["m1"],
        "the over-budget hop is the last one that connects and must be kept"
    );
    assert_eq!(proposal.proposed_order, vec!["m1"]);
    assert!(
        proposal.rationale.contains("refusing to drop the last hop"),
        "the refusal must be stated, not silently inferred from the order: {}",
        proposal.rationale
    );
}

/// The extreme of the same rule: one declared hop, over budget. There is
/// nothing to reorder and nothing safe to drop.
#[test]
fn the_proposal_refuses_to_drop_a_sole_over_budget_hop() {
    let report = assess(&[HopProbe::connected("m1", 6.5)]);
    let proposal = propose_hop_order(&report);

    assert_eq!(proposal.status, ProposalStatus::AlreadyOptimal);
    assert_eq!(proposal.proposed_order, vec!["m1"]);
    assert!(proposal.dropped.is_empty());
    assert_eq!(proposal.refused_drops, vec!["m1"]);
}

/// **Control.** A drop that is safe is actually made, so the refusal above is a
/// decision rather than an inability to drop anything.
#[test]
fn control_the_proposal_does_drop_an_over_budget_hop_when_a_healthy_one_remains() {
    let report = assess(&[
        HopProbe::connected("macmini", 6.5),
        HopProbe::connected("m1", 0.27),
    ]);
    let proposal = propose_hop_order(&report);

    assert_eq!(proposal.status, ProposalStatus::Proposed);
    assert_eq!(proposal.proposed_order, vec!["m1"]);
    assert_eq!(proposal.dropped, vec!["macmini"]);
    assert!(proposal.refused_drops.is_empty());
}

#[test]
fn the_proposal_refuses_outright_when_a_hop_was_not_measured() {
    let report = assess(&[
        HopProbe::unmeasured("macmini", Boundary::Transport),
        HopProbe::connected("m1", 0.27),
    ]);
    let proposal = propose_hop_order(&report);

    assert_eq!(proposal.status, ProposalStatus::Refused);
    assert!(!proposal.is_change());
    assert!(proposal.proposed_order.is_empty());
    assert!(proposal.dropped.is_empty());
    assert!(
        proposal.rationale.contains("is a guess"),
        "a reorder decided from an unmeasured hop is a guess: {}",
        proposal.rationale
    );
}

#[test]
fn the_proposal_refuses_when_no_hop_connects_at_all() {
    let report = assess(&[
        HopProbe::timed_out("macmini", 18.0),
        HopProbe::refused("m1", 0.05),
    ]);
    let proposal = propose_hop_order(&report);

    assert_eq!(proposal.status, ProposalStatus::Refused);
    assert!(proposal.proposed_order.is_empty());
    assert!(
        proposal.dropped.is_empty(),
        "dropping every hop is not a repair"
    );
    assert!(proposal.rationale.contains("Restore a hop"));
}

/// **Control.** A healthy relay yields no edit, so the proposal is not simply a
/// machine that always rewrites the hop list.
#[test]
fn control_a_healthy_relay_yields_no_proposed_change() {
    let report = assess(&healthy_probes());
    let proposal = propose_hop_order(&report);

    assert_eq!(proposal.status, ProposalStatus::AlreadyOptimal);
    assert!(!proposal.is_change());
    assert_eq!(proposal.proposed_order, proposal.current_order);
    assert!(proposal.dropped.is_empty());
    assert!(proposal.refused_drops.is_empty());
}
