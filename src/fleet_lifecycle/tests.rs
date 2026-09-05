//! Tests for the leak assertion.
//!
//! The governing constraint, taken from the adversarial review that stopped the
//! narrow fix from shipping: **absence must never produce success.** The design
//! that was rejected could emit exit 0 for a pull request closed *without*
//! merging, which is worse than the three-day poll it replaced. Several tests
//! here exist solely to make that unrepresentable.
//!
//! Fixtures use the real shape: a watcher on PR 7996, whose ship-state carried
//! empty evidence and no dispatched runs, and whose subject merged through the
//! merge queue — a path that never populated the local record.

use chrono::{Duration, TimeZone, Utc};

use super::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 5, 6, 0, 0).unwrap()
}

fn watcher() -> LiveObject {
    LiveObject {
        id: "watch --pr 7996 --follow".to_owned(),
        kind: "ship-state watcher".to_owned(),
        owner: "shipyard daemon".to_owned(),
        subject: SubjectRef {
            authority: "github".to_owned(),
            qualified_id: Some("Generous-Corp/pulp#7996".to_owned()),
        },
        live_since: now() - Duration::days(4),
        remedy: Some("shipyard ship-state discard 7996".to_owned()),
    }
}

fn ended(outcome: Outcome, monotonic: bool, mins_ago: i64) -> SubjectState {
    SubjectState::Terminal {
        outcome,
        monotonic,
        ended_at: now() - Duration::minutes(mins_ago),
    }
}

fn assess(local: LocalEvidence, subject: &SubjectState) -> LeakReport {
    assess_live_object(&watcher(), local, subject, LeakThresholds::default(), now())
}

// ---------------------------------------------------------------------------
// The incident
// ---------------------------------------------------------------------------

/// PR 7996: empty evidence, no dispatched runs, merged through the merge queue.
/// Nothing local could ever have marked it terminal, so the watcher polled for
/// three days and thirteen hours.
#[test]
fn the_merge_queue_shape_is_a_leak() {
    let report = assess(
        LocalEvidence::Absent,
        &ended(Outcome::Succeeded, true, 3 * 24 * 60 + 13 * 60),
    );

    assert_eq!(report.state, LeakState::Leaked);
    assert!(report.verdict.is_raise());
    assert_eq!(report.outcome, Some(Outcome::Succeeded));
    assert!(report.detail.contains("outlived"), "{}", report.detail);
    assert!(
        report.next_action.contains("ship-state discard"),
        "the report must name the command that ends it: {}",
        report.next_action
    );
    assert!(
        report.detail.contains("shipyard daemon"),
        "an alert that omits the owner makes its reader restart the search: {}",
        report.detail
    );
}

/// Planted control: the identical object while its subject is still open. If
/// this went red the leak rule would just be flagging every live object.
#[test]
fn control_an_object_tracking_a_live_subject_is_not_a_leak() {
    let report = assess(LocalEvidence::Absent, &SubjectState::Live);

    assert_eq!(report.state, LeakState::Tracking);
    assert!(!report.verdict.is_raise());
}

#[test]
fn an_object_inside_its_winddown_grace_is_not_yet_a_leak() {
    let report = assess(LocalEvidence::InFlight, &ended(Outcome::Succeeded, true, 1));
    assert_eq!(report.state, LeakState::WindingDown);
    assert!(!report.verdict.is_raise());
}

/// Pairs with the test above: the same object past the grace. Together they
/// prove the trigger is the duration and not the mere fact of ending.
#[test]
fn control_the_same_object_past_the_grace_is_a_leak() {
    let report = assess(
        LocalEvidence::InFlight,
        &ended(Outcome::Succeeded, true, 60),
    );
    assert_eq!(report.state, LeakState::Leaked);
    assert!(report.verdict.is_raise());
}

// ---------------------------------------------------------------------------
// Absence must never produce success — the rejected design's fatal flaw
// ---------------------------------------------------------------------------

/// The blocking finding that stopped the narrow fix: it could report success
/// for a pull request closed **without** merging. Nothing here may do that.
#[test]
fn negative_control_a_subject_that_closed_unmerged_is_never_a_success() {
    let report = assess(
        LocalEvidence::Complete { passed: true },
        &ended(Outcome::Failed, false, 60),
    );

    assert!(!report.succeeded(), "closed-unmerged is not a success");
    assert_eq!(report.outcome, Some(Outcome::Failed));
    assert!(report.verdict.is_raise());
}

/// Exhaustive: success is reachable from exactly one observation, and every
/// other state — including both ways of not being able to see — is not it.
#[test]
fn negative_control_success_is_reachable_only_from_an_observed_success() {
    let unreadable = SubjectState::Unreadable {
        boundary: Boundary::Transport,
    };
    let cases = [
        (SubjectState::Live, false),
        (ended(Outcome::Succeeded, true, 60), true),
        (ended(Outcome::Failed, false, 60), false),
        (ended(Outcome::Abandoned, false, 60), false),
        (unreadable, false),
    ];

    for (subject, expected) in cases {
        for local in [
            LocalEvidence::Absent,
            LocalEvidence::InFlight,
            LocalEvidence::Complete { passed: true },
            LocalEvidence::Complete { passed: false },
        ] {
            let report = assess(local, &subject);
            assert_eq!(
                report.succeeded(),
                expected,
                "subject {subject:?} with local {} must not read as success={}",
                local.as_str(),
                report.succeeded()
            );
        }
    }
}

/// An unreadable authority is `Unknown`, never terminal and never a pass.
#[test]
fn negative_control_an_unreadable_subject_makes_no_claim() {
    let report = assess(
        LocalEvidence::Complete { passed: true },
        &SubjectState::Unreadable {
            boundary: Boundary::Transport,
        },
    );

    assert_eq!(report.state, LeakState::Undetermined);
    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Transport));
    assert_eq!(report.outcome, None);
    assert!(!report.succeeded());
}

/// An under-qualified reference resolves against an ambient default and can
/// report an unrelated subject as ended. Refuse before probing.
#[test]
fn negative_control_an_unqualified_subject_reference_is_refused() {
    let mut object = watcher();
    object.subject.qualified_id = None;

    let report = assess_live_object(
        &object,
        LocalEvidence::Absent,
        &ended(Outcome::Succeeded, true, 9999),
        LeakThresholds::default(),
        now(),
    );

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Scope));
    assert_eq!(report.outcome, None, "a wrong terminal is worse than none");
    assert!(!report.succeeded());
}

// ---------------------------------------------------------------------------
// Revocability, and not inheriting a terminal
// ---------------------------------------------------------------------------

/// Merged is monotonic: a later read that fails to see it cannot resurrect it.
#[test]
fn a_monotonic_terminal_is_never_revoked() {
    let merged = ended(Outcome::Succeeded, true, 60);
    let reconciled = reconcile_subject(Some(&merged), SubjectState::Live);
    assert_eq!(reconciled, merged);
}

/// Closed is not: pull requests reopen, and a terminal that cannot be revoked
/// would strand a genuinely live subject as permanently ended — leaking in the
/// opposite direction by ending an object whose subject is running.
#[test]
fn control_a_revocable_terminal_yields_to_a_reopened_subject() {
    let closed = ended(Outcome::Failed, false, 60);
    let reconciled = reconcile_subject(Some(&closed), SubjectState::Live);
    assert_eq!(reconciled, SubjectState::Live);
}

/// A replacement must not be born terminal and reaped before doing any work.
#[test]
fn negative_control_a_successor_never_inherits_a_terminal() {
    assert_eq!(subject_state_for_successor(), None);
}

// ---------------------------------------------------------------------------
// The losing signal is recorded, never dropped
// ---------------------------------------------------------------------------

/// Exhaustive sweep: whenever the authority answered at all, the comparison is
/// on the report. A rule that silently drops the loser is how a false terminal
/// becomes impossible to debug afterwards.
#[test]
fn the_losing_signal_is_always_recorded() {
    let subjects = [
        SubjectState::Live,
        ended(Outcome::Succeeded, true, 60),
        ended(Outcome::Failed, false, 60),
        ended(Outcome::Abandoned, false, 60),
        SubjectState::Unreadable {
            boundary: Boundary::Transport,
        },
    ];
    for subject in &subjects {
        for local in [
            LocalEvidence::Absent,
            LocalEvidence::InFlight,
            LocalEvidence::Complete { passed: true },
            LocalEvidence::Complete { passed: false },
        ] {
            let report = assess(local, subject);
            let recorded = report
                .disagreement
                .as_ref()
                .expect("every answered comparison is recorded");
            assert_eq!(recorded.local, local);
            assert!(
                !recorded.loser_signal.is_empty(),
                "the loser must be preserved verbatim"
            );
        }
    }
}

/// The authority decides terminality in both directions. Local evidence that
/// passed does not end anything.
#[test]
fn local_completion_never_terminates_an_open_subject() {
    for passed in [true, false] {
        let report = assess(LocalEvidence::Complete { passed }, &SubjectState::Live);
        assert_eq!(report.state, LeakState::Tracking);
        assert_eq!(report.outcome, None);
        assert!(!report.succeeded());
        let recorded = report.disagreement.expect("recorded");
        assert_eq!(recorded.winner, Authority::Subject);
        assert!(recorded.conflicting);
        assert!(
            !recorded.raises,
            "waiting on review or the queue is the everyday case and must stay quiet"
        );
        assert!(recorded.loser_signal.contains("NOT terminal"));
    }
}

/// Some disagreements are findings in their own right, independent of whether
/// the object leaked.
#[test]
fn a_subject_that_landed_past_a_failing_gate_raises_on_its_own() {
    let report = assess(
        LocalEvidence::Complete { passed: false },
        &ended(Outcome::Succeeded, true, 1),
    );

    // Inside the grace, so the object itself is fine.
    assert_eq!(report.state, LeakState::WindingDown);
    assert!(!report.verdict.is_raise());

    // The disagreement is still a finding.
    let recorded = report.disagreement.expect("recorded");
    assert!(
        recorded.raises,
        "landing past a failing gate is worth saying"
    );
    assert!(recorded.loser_signal.contains("failing gate"));
}

#[test]
fn a_passing_gate_that_did_not_land_raises_on_its_own() {
    let report = assess(
        LocalEvidence::Complete { passed: true },
        &ended(Outcome::Abandoned, false, 1),
    );
    let recorded = report.disagreement.expect("recorded");
    assert!(recorded.raises);
    assert!(recorded.loser_signal.contains("PASSED"));
}

/// The measured shape: local terminality was unreachable, so the fact that only
/// the authority knew is itself worth recording.
#[test]
fn absent_local_evidence_against_an_ended_subject_is_flagged() {
    let report = assess(
        LocalEvidence::Absent,
        &ended(Outcome::Succeeded, true, 9999),
    );
    let recorded = report.disagreement.expect("recorded");
    assert!(recorded.raises);
    assert!(recorded.loser_signal.contains("nothing local could ever"));
}

/// Control for the three tests above: agreement must NOT raise, or "raises"
/// would carry no information.
#[test]
fn control_agreement_between_the_two_sources_does_not_raise() {
    for (local, subject) in [
        (LocalEvidence::InFlight, SubjectState::Live),
        (
            LocalEvidence::Complete { passed: true },
            ended(Outcome::Succeeded, true, 1),
        ),
        (
            LocalEvidence::Complete { passed: false },
            ended(Outcome::Failed, false, 1),
        ),
    ] {
        let report = assess(local, &subject);
        let recorded = report.disagreement.expect("recorded");
        assert!(
            !recorded.raises,
            "agreement must be quiet: {}",
            recorded.loser_signal
        );
    }
}

// ---------------------------------------------------------------------------
// Roll-up
// ---------------------------------------------------------------------------

#[test]
fn negative_control_an_empty_roll_up_is_unknown_not_served() {
    assert_eq!(roll_up(&[]), ServiceVerdict::Unknown);
}

#[test]
fn the_roll_up_takes_the_worst() {
    let healthy = assess(LocalEvidence::InFlight, &SubjectState::Live);
    let leaked = assess(
        LocalEvidence::Absent,
        &ended(Outcome::Succeeded, true, 9999),
    );
    assert_eq!(roll_up(&[healthy, leaked]), ServiceVerdict::Degraded);
}

#[test]
fn state_and_outcome_strings_are_stable() {
    assert_eq!(LeakState::Tracking.as_str(), "tracking");
    assert_eq!(LeakState::WindingDown.as_str(), "winding_down");
    assert_eq!(LeakState::Leaked.as_str(), "leaked");
    assert_eq!(LeakState::Undetermined.as_str(), "undetermined");
    assert_eq!(Outcome::Succeeded.as_str(), "succeeded");
    assert_eq!(Outcome::Failed.as_str(), "failed");
    assert_eq!(Outcome::Abandoned.as_str(), "abandoned");
    assert_eq!(LocalEvidence::Absent.as_str(), "absent");
    assert_eq!(
        LocalEvidence::Complete { passed: true }.as_str(),
        "complete_pass"
    );
}

// ---------------------------------------------------------------------------
// Quadrant 3: subject ended, local work still running — cancel or drain
// ---------------------------------------------------------------------------

fn work(budget: RemainingBudget, wanted: bool) -> LocalWork {
    LocalWork {
        description: "macOS validation leg".to_owned(),
        budget,
        output_still_wanted: wanted,
    }
}

fn reclaim(w: &LocalWork) -> Reclamation {
    decide_reclamation(w, LeakThresholds::default())
}

/// Bounded, cheap, and still wanted: let it finish and keep the evidence.
#[test]
fn bounded_cheap_work_that_is_still_wanted_is_drained() {
    let decision = reclaim(&work(
        RemainingBudget::Bounded {
            secs_remaining: 120,
        },
        true,
    ));
    assert_eq!(decision.kind(), "drain");
}

/// Planted control for the test above: the same work, past the drain budget.
/// Only the remaining cost differs.
#[test]
fn control_bounded_but_expensive_work_is_cancelled() {
    let decision = reclaim(&work(
        RemainingBudget::Bounded {
            secs_remaining: 6000,
        },
        true,
    ));
    assert_eq!(decision.kind(), "cancel");
}

/// Unbounded work cannot be drained by definition — that is how a leak becomes
/// permanent.
#[test]
fn unbounded_work_is_cancelled_not_drained() {
    assert_eq!(
        reclaim(&work(RemainingBudget::Unbounded, true)).kind(),
        "cancel"
    );
}

/// An absence of measurement is not a licence to cancel work somebody may still
/// need. This is the same rule the self-heal gate applies to idle proof.
#[test]
fn negative_control_unmeasured_work_is_escalated_not_cancelled() {
    let decision = reclaim(&work(RemainingBudget::Unknown, true));
    assert_eq!(decision.kind(), "escalate");
    match decision {
        Reclamation::Escalate { reason } => {
            assert!(reason.contains("not a licence to cancel"), "{reason}");
        }
        other => panic!("expected escalate, got {other:?}"),
    }
}

/// Output nobody wants is cancelled regardless of how cheap it would be to
/// finish — cheapness is not a reason to do useless work.
#[test]
fn work_whose_output_is_unwanted_is_cancelled_however_cheap() {
    let decision = reclaim(&work(RemainingBudget::Bounded { secs_remaining: 1 }, false));
    assert_eq!(decision.kind(), "cancel");
}

/// Whatever is chosen, it is stated. Silently abandoning the work is the leak
/// in miniature, so there is no variant that means "do nothing and say nothing".
#[test]
fn every_reclamation_states_a_reason() {
    for w in [
        work(RemainingBudget::Bounded { secs_remaining: 60 }, true),
        work(
            RemainingBudget::Bounded {
                secs_remaining: 9999,
            },
            true,
        ),
        work(RemainingBudget::Unbounded, true),
        work(RemainingBudget::Unknown, true),
        work(RemainingBudget::Bounded { secs_remaining: 60 }, false),
    ] {
        let decision = reclaim(&w);
        let reason = match &decision {
            Reclamation::Drain { reason }
            | Reclamation::Cancel { reason }
            | Reclamation::Escalate { reason } => reason,
        };
        assert!(
            !reason.is_empty() && reason.contains("macOS validation leg"),
            "{}: {reason}",
            decision.kind()
        );
    }
}
