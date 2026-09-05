//! Tests for the fail-closed handback obligation.
//!
//! Fixtured on the incident: a process exited because it could not prove it had
//! deleted its VM, raised nothing, and left a failed lease release unresolved.

use super::*;

fn held_lease() -> HeldResource {
    HeldResource {
        kind: "lease".to_owned(),
        id: "tartci/m3/macos-1".to_owned(),
        owner_after_exit: None,
    }
}

fn handed_over_lease() -> HeldResource {
    HeldResource {
        owner_after_exit: Some("tartci-reaper".to_owned()),
        ..held_lease()
    }
}

// ---------------------------------------------------------------------------
// The incident
// ---------------------------------------------------------------------------

#[test]
fn the_silent_fail_closed_exit_owes_on_every_count_it_should() {
    let report = assess_exit(true, None, &[held_lease()], RetryPolicy::None);

    assert_eq!(report.verdict, HandbackVerdict::Owing);
    let names: Vec<&str> = report.unmet.iter().map(Unmet::as_str).collect();
    assert_eq!(names, vec!["dispose", "raise"]);
    assert!(
        report.summary.contains("may have been correct"),
        "{}",
        report.summary
    );
}

/// **The load-bearing assertion of this module.** Being right is not a
/// discharge. The refusal here is correct and the exit still owes an
/// escalation — if this ever passed, the module would be scoring intent
/// instead of obligation.
#[test]
fn a_correct_refusal_still_owes_an_escalation() {
    let report = assess_exit(true, None, &[], RetryPolicy::None);
    assert_eq!(report.verdict, HandbackVerdict::Owing);
    assert_eq!(report.unmet, vec![Unmet::Raise]);
}

/// The control that stops the above condemning every refusal: raise it, hold
/// nothing, do not retry blindly, and the same fail-closed exit is discharged.
/// Without this, "a refusal owes" would also pass for a module that faulted
/// unconditionally.
#[test]
fn control_a_refusal_that_raises_and_disposes_is_discharged() {
    let report = assess_exit(
        true,
        Some("shipyard#577"),
        &[handed_over_lease()],
        RetryPolicy::None,
    );
    assert_eq!(report.verdict, HandbackVerdict::Discharged);
    assert!(report.unmet.is_empty(), "{:?}", report.unmet);
}

/// An empty escalation reference is not an escalation. Treating `Some("")` as
/// discharged would let a caller satisfy the contract by passing a blank.
#[test]
fn negative_control_an_empty_escalation_reference_does_not_count_as_raising() {
    let report = assess_exit(true, Some(""), &[], RetryPolicy::None);
    assert_eq!(report.unmet, vec![Unmet::Raise]);
}

// ---------------------------------------------------------------------------
// Repeating an unproven action is not recovery
// ---------------------------------------------------------------------------

/// The crash-loop trap: an unbounded restart into a state the process could not
/// prove is the `NRestarts=36088` pattern, and it is worse than staying down
/// because it does damage on a timer.
#[test]
fn an_unbounded_retry_into_unproven_state_is_unmet_and_ranks_first() {
    let report = assess_exit(true, Some("shipyard#577"), &[], RetryPolicy::Unbounded);
    assert_eq!(report.verdict, HandbackVerdict::Owing);
    assert_eq!(report.unmet.first(), Some(&Unmet::Bound));
}

/// Control: a bounded retry is fine. Without it, "unbounded is unmet" would
/// also pass for a module that rejected every retry policy and would push
/// callers toward never retrying at all.
#[test]
fn control_a_bounded_retry_is_not_an_unmet_obligation() {
    let report = assess_exit(
        true,
        Some("shipyard#577"),
        &[],
        RetryPolicy::Bounded { attempts: 3 },
    );
    assert_eq!(report.verdict, HandbackVerdict::Discharged);
}

/// The full incident shape, plus the restart policy that would have made it
/// worse. All three obligations unmet, ordered by consequence.
#[test]
fn the_worst_case_reports_every_obligation_most_consequential_first() {
    let report = assess_exit(true, None, &[held_lease()], RetryPolicy::Unbounded);
    let names: Vec<&str> = report.unmet.iter().map(Unmet::as_str).collect();
    assert_eq!(names, vec!["bound", "dispose", "raise"]);
}

// ---------------------------------------------------------------------------
// Silence is correct when there is nothing to say
// ---------------------------------------------------------------------------

/// A clean exit owes no escalation. Faulting on every quiet success would bury
/// the real findings under the ordinary ones — the same noise argument that
/// makes escalation dry-run by default.
#[test]
fn negative_control_a_clean_exit_holding_nothing_owes_nothing() {
    let report = assess_exit(false, None, &[], RetryPolicy::None);
    assert_eq!(report.verdict, HandbackVerdict::Discharged);
    assert!(report.unmet.is_empty());
}

/// But a clean exit that walked away from a resource still leaks. The exit code
/// says nothing about whether the lease was released.
#[test]
fn a_clean_exit_that_orphans_a_resource_still_owes_a_disposition() {
    let report = assess_exit(false, None, &[held_lease()], RetryPolicy::None);
    assert_eq!(report.verdict, HandbackVerdict::Owing);
    assert!(matches!(report.unmet.as_slice(), [Unmet::Dispose { .. }]));
    // It must not be charged for failing to raise: it had nothing to raise.
    assert!(!report.unmet.contains(&Unmet::Raise));
}

#[test]
fn a_resource_with_a_named_successor_is_not_orphaned() {
    assert!(!handed_over_lease().is_orphaned());
    assert!(held_lease().is_orphaned());

    let report = assess_exit(
        true,
        Some("shipyard#577"),
        &[handed_over_lease()],
        RetryPolicy::None,
    );
    assert!(
        !report
            .unmet
            .iter()
            .any(|u| matches!(u, Unmet::Dispose { .. })),
        "{:?}",
        report.unmet
    );
}

#[test]
fn the_orphaned_resources_are_reported_so_a_human_can_find_them() {
    let report = assess_exit(
        true,
        None,
        &[held_lease(), handed_over_lease()],
        RetryPolicy::None,
    );
    let Some(Unmet::Dispose { orphaned }) = report
        .unmet
        .iter()
        .find(|u| matches!(u, Unmet::Dispose { .. }))
    else {
        panic!("expected a dispose obligation: {:?}", report.unmet);
    };
    assert_eq!(orphaned.len(), 1, "only the un-owned one is orphaned");
    assert_eq!(orphaned[0].id, "tartci/m3/macos-1");
}

// ---------------------------------------------------------------------------
// An unobservable exit is not a discharged one
// ---------------------------------------------------------------------------

/// The separate constructor exists so an exit nobody could read cannot be
/// scored `Discharged` by passing defaults into `assess_exit`.
#[test]
fn negative_control_an_unobservable_exit_is_unknown_not_discharged() {
    let report = unobservable_exit(
        Boundary::Transport,
        "the host stopped answering mid-teardown",
    );
    assert!(matches!(
        report.verdict,
        HandbackVerdict::Unknown {
            boundary: Boundary::Transport,
            ..
        }
    ));
    assert_ne!(report.verdict, HandbackVerdict::Discharged);
}
