//! Tests for the survivability assertion.
//!
//! The fixture throughout is the real 2026-09-05 incident: `Shipyard-studio-02`
//! on M3, the only macOS runner for this repository, which a routine
//! force-push permanently removed. Every assertion is paired with a control,
//! because the failure this module guards against is a detector that reports a
//! fault it cannot actually distinguish from a blind spot.

use super::*;

/// `Shipyard-studio-02` exactly as observed: persistent, service installed,
/// **not** loaded, and no restart policy.
fn studio_02_as_found() -> RunnerSupervision {
    RunnerSupervision {
        name: "Shipyard-studio-02".to_owned(),
        ephemeral: Some(false),
        has_registrar: None,
        service_label: Some("actions.runner.danielraffel-Shipyard.Shipyard-studio-02".to_owned()),
        loaded_in_supervisor: Some(false),
        restart_on_exit: Some(false),
    }
}

// ---------------------------------------------------------------------------
// The incident, and the control that proves the detector is not always-on
// ---------------------------------------------------------------------------

#[test]
fn the_runner_that_a_force_push_removed_reads_as_unsupervised() {
    let verdict = assess_restartability(&studio_02_as_found());
    assert!(verdict.is_fault(), "{verdict:?}");
    assert_eq!(verdict.as_str(), "unsupervised");
}

/// Control. The *only* difference from the incident fixture is that the job is
/// loaded and declares a restart policy. If this also read as a fault, the
/// assertion above would prove nothing about supervision — it would just be a
/// detector that always fires.
#[test]
fn control_the_same_runner_with_keepalive_is_supervised_and_raises_nothing() {
    let healthy = RunnerSupervision {
        loaded_in_supervisor: Some(true),
        restart_on_exit: Some(true),
        ..studio_02_as_found()
    };
    let verdict = assess_restartability(&healthy);
    assert!(!verdict.is_fault(), "{verdict:?}");
    assert_eq!(verdict.as_str(), "supervised");
}

/// Loaded but with no restart policy is still a fault: `RunAtLoad` starts a job
/// once and says nothing about what happens when it exits. This is the precise
/// shape of the incident plist, and the easiest one to wave through.
#[test]
fn loaded_without_a_restart_policy_is_still_unsupervised() {
    let loaded_no_keepalive = RunnerSupervision {
        loaded_in_supervisor: Some(true),
        restart_on_exit: Some(false),
        ..studio_02_as_found()
    };
    assert!(assess_restartability(&loaded_no_keepalive).is_fault());
}

// ---------------------------------------------------------------------------
// The rule the module exists for: unreadable is not broken
// ---------------------------------------------------------------------------

/// **The planted control for this module's whole reason to exist.**
///
/// A LaunchAgent lives in the per-user GUI domain, so `launchctl list` over SSH
/// returns nothing for a job that is loaded and running. Reading that silence
/// as "not loaded" files a fault against a healthy runner. An unqueryable
/// domain must yield `Unknown`, and must not raise.
#[test]
fn negative_control_an_unqueryable_supervisor_domain_is_unknown_not_a_fault() {
    let could_not_read = RunnerSupervision {
        loaded_in_supervisor: None,
        ..studio_02_as_found()
    };
    let verdict = assess_restartability(&could_not_read);

    assert!(
        !verdict.is_fault(),
        "an unreadable domain must never be reported as unsupervised: {verdict:?}"
    );
    assert!(
        matches!(
            verdict,
            Restartability::Unknown {
                boundary: Boundary::Scope,
                ..
            }
        ),
        "{verdict:?}"
    );
}

/// Pairing control. Without it, "unreadable is Unknown" would also pass for an
/// implementation that answered `Unknown` to everything and could therefore
/// never report the incident it was written for.
#[test]
fn control_a_readable_domain_still_reaches_a_verdict() {
    assert!(matches!(
        assess_restartability(&studio_02_as_found()),
        Restartability::Unsupervised { .. }
    ));
}

#[test]
fn an_unreadable_registration_is_unknown_rather_than_assumed_persistent() {
    let unreadable = RunnerSupervision {
        ephemeral: None,
        ..studio_02_as_found()
    };
    assert!(matches!(
        assess_restartability(&unreadable),
        Restartability::Unknown {
            boundary: Boundary::Parse,
            ..
        }
    ));
}

#[test]
fn a_loaded_job_with_an_unreadable_restart_policy_is_unknown_not_a_fault() {
    let verdict = assess_restartability(&RunnerSupervision {
        loaded_in_supervisor: Some(true),
        restart_on_exit: None,
        ..studio_02_as_found()
    });
    assert!(!verdict.is_fault(), "{verdict:?}");
    assert!(matches!(
        verdict,
        Restartability::Unknown {
            boundary: Boundary::Parse,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Ephemeral runners exit by design — that must not read as a fault
// ---------------------------------------------------------------------------

#[test]
fn an_ephemeral_runner_with_a_registrar_is_self_replacing_not_a_fault() {
    let ephemeral = RunnerSupervision {
        name: "pulp-auto-ephemeral-200".to_owned(),
        ephemeral: Some(true),
        has_registrar: Some(true),
        ..RunnerSupervision::default()
    };
    let verdict = assess_restartability(&ephemeral);
    assert!(!verdict.is_fault(), "{verdict:?}");
    assert_eq!(verdict.as_str(), "self_replacing");
}

/// The mirror: ephemeral with nothing creating replacements drains to zero one
/// job at a time. Slower than the incident, identical destination.
#[test]
fn an_ephemeral_runner_with_no_registrar_is_a_fault() {
    let stranded = RunnerSupervision {
        name: "stranded".to_owned(),
        ephemeral: Some(true),
        has_registrar: Some(false),
        ..RunnerSupervision::default()
    };
    assert!(assess_restartability(&stranded).is_fault());
}

// ---------------------------------------------------------------------------
// Lane roll-up
// ---------------------------------------------------------------------------

/// Capacity one, unsupervised: the actual state of `macOS ARM64 [local]`.
#[test]
fn the_sole_unsupervised_runner_makes_the_lane_a_single_point_of_failure() {
    let report = assess_lane_survivability("macOS ARM64 [local]", &[studio_02_as_found()]);
    assert_eq!(report.verdict, Survivability::SinglePointOfFailure);
    assert!(report.verdict.should_raise());
    assert!(report.summary.contains("permanently"), "{}", report.summary);
}

/// Control: add one supervised sibling and the same lane is only `Fragile`.
/// This is what a second macOS runner would have bought on the night.
#[test]
fn control_a_supervised_sibling_downgrades_the_lane_to_fragile() {
    let sibling = RunnerSupervision {
        name: "Shipyard-studio-01".to_owned(),
        loaded_in_supervisor: Some(true),
        restart_on_exit: Some(true),
        ..studio_02_as_found()
    };
    let report = assess_lane_survivability("macOS ARM64 [local]", &[studio_02_as_found(), sibling]);
    assert_eq!(report.verdict, Survivability::Fragile);
    assert!(report.verdict.should_raise());
}

#[test]
fn an_all_supervised_lane_is_survivable_and_raises_nothing() {
    let good = RunnerSupervision {
        loaded_in_supervisor: Some(true),
        restart_on_exit: Some(true),
        ..studio_02_as_found()
    };
    let report = assess_lane_survivability("macOS ARM64 [local]", &[good]);
    assert_eq!(report.verdict, Survivability::Survivable);
    assert!(!report.verdict.should_raise());
}

/// An empty census is the signature of a scope error, which is how the org
/// runner query already misled this fleet once. It must not become the
/// module's loudest verdict.
#[test]
fn negative_control_an_empty_census_is_unknown_not_a_single_point_of_failure() {
    let report = assess_lane_survivability("macOS ARM64 [local]", &[]);
    assert_eq!(report.verdict, Survivability::Unknown);
    assert!(!report.verdict.should_raise());
}

#[test]
fn a_lane_whose_supervision_is_entirely_unreadable_is_unknown() {
    let blind = RunnerSupervision {
        loaded_in_supervisor: None,
        ..studio_02_as_found()
    };
    let report = assess_lane_survivability("macOS ARM64 [local]", &[blind]);
    assert_eq!(report.verdict, Survivability::Unknown);
    assert!(!report.verdict.should_raise());
}
