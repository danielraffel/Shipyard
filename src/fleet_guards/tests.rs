//! Tests for the fleet guard assertions.
//!
//! The paths, unit names and numbers below are the ones observed on the hosts
//! these assertions were written against — `NRestarts=36089` and a 227-line
//! host-side delta are measurements, not illustrations — so each fixture
//! reproduces the shape of the incident rather than an idealisation of it.
//!
//! Every check carries a **planted negative control that must go red**, and
//! each is paired with the healthy fixture that must stay green, because a
//! detector that cannot fail its own test is exactly the failure mode this
//! module exists to end. Two pairs are load-bearing and kept adjacent:
//!
//! * `enabled with no next elapse` versus `enabled with one` — without the
//!   second, the arming assertion could be one that always raises;
//! * a high absolute `NRestarts` with **zero** delta versus a small absolute
//!   with a delta over the ceiling — together they prove the trigger is the
//!   rate and not the level, which is the whole point of the restart check.

use chrono::{Duration, TimeZone, Utc};

use super::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 4, 23, 44, 0).unwrap()
}

fn thresholds() -> RestartChurnThresholds {
    RestartChurnThresholds::default()
}

fn timer(presence: UnitPresence, next_elapse: Option<DateTime<Utc>>) -> UnitObservation {
    UnitObservation {
        unit: "tartci-reaper.timer".to_owned(),
        kind: UnitKind::Timer,
        presence,
        next_elapse,
        restarts: None,
    }
}

fn service(presence: UnitPresence) -> UnitObservation {
    UnitObservation {
        unit: "tartci-reaper.service".to_owned(),
        kind: UnitKind::Service,
        presence,
        next_elapse: None,
        restarts: None,
    }
}

fn enabled() -> UnitPresence {
    UnitPresence::Present {
        state: EnablementState::Enabled,
    }
}

fn disabled() -> UnitPresence {
    UnitPresence::Present {
        state: EnablementState::Disabled,
    }
}

fn armed_timer() -> UnitObservation {
    timer(enabled(), Some(now() + Duration::minutes(7)))
}

fn guard(payload: PayloadPresence, units: Vec<UnitObservation>) -> GuardObservation {
    GuardObservation {
        name: "tartci-reaper".to_owned(),
        payload_path: "/usr/local/bin/tartci-reaper.py".to_owned(),
        payload,
        units,
    }
}

fn assess_guard(observation: &GuardObservation) -> GuardArmingReport {
    assess_guard_arming(observation, thresholds(), now())
}

fn assess_unit(observation: &UnitObservation) -> UnitArmingReport {
    assess_unit_arming(observation, thresholds(), now())
}

fn churn(current: u64, baseline: Option<(u64, i64)>) -> RestartReport {
    let observation = RestartObservation {
        current,
        baseline: baseline.map(|(count, minutes_ago)| RestartBaseline {
            count,
            observed_at: now() - Duration::minutes(minutes_ago),
        }),
    };
    assess_restart_churn(&observation, thresholds(), now())
}

// ---------------------------------------------------------------------------
// Incident A — a guard in the repo is not a guard until it is armed on the host
// ---------------------------------------------------------------------------

/// The nineteen-day outage, exactly: the script was on the host and the units
/// that would have invoked it were not.
#[test]
fn negative_control_script_installed_but_units_absent_raises() {
    let report = assess_guard(&guard(
        PayloadPresence::Installed,
        vec![
            service(UnitPresence::Absent),
            timer(UnitPresence::Absent, None),
        ],
    ));

    assert_eq!(report.verdict, ServiceVerdict::Unserved);
    assert!(report.verdict.is_raise());
    assert_eq!(report.armed_units, 0);
    assert!(
        report.detail.contains("never invoked"),
        "the guard-level detail must name the pair — payload present, nothing \
         invoking it: {}",
        report.detail
    );
    assert!(report.detail.contains("/usr/local/bin/tartci-reaper.py"));
}

/// The positive control for the pair above. Without it the assertion could be
/// one that raises no matter what it is handed.
#[test]
fn positive_control_a_fully_armed_guard_is_served_and_does_not_raise() {
    let report = assess_guard(&guard(
        PayloadPresence::Installed,
        vec![
            service(UnitPresence::Present {
                state: EnablementState::Static,
            }),
            armed_timer(),
        ],
    ));

    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert!(!report.verdict.is_raise());
    assert_eq!(report.armed_units, 2);
    assert!(report.units.iter().all(|unit| unit.armed));
}

/// Units armed but nothing for them to run is the same outage from the other
/// end, and must not read as healthy just because the units are fine.
#[test]
fn negative_control_armed_units_with_an_absent_payload_raise() {
    let report = assess_guard(&guard(PayloadPresence::Absent, vec![armed_timer()]));

    assert_eq!(report.verdict, ServiceVerdict::Unserved);
    assert!(report.detail.contains("payload they invoke is absent"));
}

/// Asserting over an empty unit set is not the same as asserting it passed.
#[test]
fn negative_control_a_guard_declaring_no_units_is_unknown_not_served() {
    let report = assess_guard(&guard(PayloadPresence::Installed, Vec::new()));

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Scope));
}

/// An uninspectable payload is not an armed guard.
#[test]
fn negative_control_an_unreadable_payload_is_unknown_with_its_boundary() {
    let report = assess_guard(&guard(
        PayloadPresence::Unreadable {
            boundary: Boundary::Permission,
        },
        vec![armed_timer()],
    ));

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Permission));
}

// ---------------------------------------------------------------------------
// `enabled` is not `armed` — the distinction, and that it stays legible
// ---------------------------------------------------------------------------

/// A timer that is enabled and has no next elapse fires never, which is exactly
/// as useless as not being installed — and it reports `enabled` throughout.
#[test]
fn negative_control_timer_enabled_with_no_next_elapse_raises() {
    let report = assess_unit(&timer(enabled(), None));

    assert_eq!(report.arming, ArmingState::NoNextElapse);
    assert_eq!(report.verdict, ServiceVerdict::Unserved);
    assert!(report.verdict.is_raise());
    assert!(!report.armed);
    assert!(report.detail.contains("no next elapse"));
}

/// The positive control for the check above: a timer with an elapse is served.
#[test]
fn positive_control_timer_enabled_with_a_next_elapse_is_served() {
    let report = assess_unit(&armed_timer());

    assert_eq!(report.arming, ArmingState::Armed);
    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert!(!report.verdict.is_raise());
    assert!(report.armed);
}

/// "Enabled but never fires" and "not enabled" are different faults with
/// different fixes, and must not collapse into one message.
#[test]
fn no_next_elapse_is_distinguishable_from_not_enabled() {
    let no_elapse = assess_unit(&timer(enabled(), None));
    let not_enabled = assess_unit(&timer(disabled(), None));

    assert_ne!(no_elapse.arming, not_enabled.arming);
    assert_eq!(not_enabled.arming, ArmingState::NotEnabled);

    assert!(no_elapse.detail.contains("no next elapse"));
    assert!(!no_elapse.detail.contains("not enabled"));

    assert!(not_enabled.detail.contains("not enabled"));
    assert!(!not_enabled.detail.contains("no next elapse"));
}

/// Not installed, not enabled, and enabled-but-never-firing are three states,
/// not one boolean.
#[test]
fn the_four_arming_states_are_separate_values() {
    assert_eq!(
        assess_unit(&timer(UnitPresence::Absent, None)).arming,
        ArmingState::NotInstalled
    );
    assert_eq!(
        assess_unit(&timer(disabled(), None)).arming,
        ArmingState::NotEnabled
    );
    assert_eq!(
        assess_unit(&timer(enabled(), None)).arming,
        ArmingState::NoNextElapse
    );
    assert_eq!(assess_unit(&armed_timer()).arming, ArmingState::Armed);
}

/// A masked unit can never start, and says so rather than reading as merely
/// "not enabled".
#[test]
fn negative_control_a_masked_unit_raises_and_names_masking() {
    let report = assess_unit(&timer(
        UnitPresence::Present {
            state: EnablementState::Masked,
        },
        Some(now() + Duration::minutes(3)),
    ));

    assert_eq!(report.arming, ArmingState::Masked);
    assert!(report.verdict.is_raise());
    assert!(report.detail.contains("masked"));
}

/// A `static` service carries no `[Install]` section, so demanding `enabled` of
/// it would fail every healthy host.
#[test]
fn a_static_service_is_armed_by_its_timer_not_by_being_enabled() {
    let report = assess_unit(&service(UnitPresence::Present {
        state: EnablementState::Static,
    }));

    assert_eq!(report.arming, ArmingState::Armed);
    assert_eq!(report.verdict, ServiceVerdict::Served);
}

/// A unit that could not be inspected is not an armed unit.
#[test]
fn negative_control_an_unreadable_unit_is_unknown_with_its_boundary() {
    let report = assess_unit(&timer(
        UnitPresence::Unreadable {
            boundary: Boundary::Transport,
        },
        None,
    ));

    assert_eq!(report.arming, ArmingState::Undetermined);
    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Transport));
}

// ---------------------------------------------------------------------------
// `NRestarts` — the level is context, the rate is the trigger
// ---------------------------------------------------------------------------

/// The single most important test here. This is the live state of the host that
/// crash-looped to 36088: repaired, healthy, and still reporting a six-figure
/// monotonic counter. An implementation that triggers on the absolute value is
/// permanently red here, which is operationally identical to no alarm at all.
#[test]
fn high_absolute_restart_count_with_zero_delta_does_not_raise() {
    let report = churn(36_089, Some((36_089, 60)));

    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert!(!report.verdict.is_raise());
    assert_eq!(report.delta, Some(0));
    assert_eq!(report.current, 36_089);
    assert!(report.detail.contains("monotonic"));
}

/// The negative control for the test above.
#[test]
fn negative_control_small_absolute_with_a_delta_over_the_ceiling_raises() {
    let report = churn(4, Some((0, 5)));

    assert_eq!(report.verdict, ServiceVerdict::Degraded);
    assert!(report.verdict.is_raise());
    assert_eq!(report.delta, Some(4));
    assert_eq!(report.projected_per_window, Some(48));
}

/// The pair, asserted together: the raising case has an absolute counter four
/// orders of magnitude *smaller* than the passing one. Nothing but the rate can
/// explain that, which is the property this check exists to hold.
#[test]
fn the_restart_trigger_is_the_delta_and_not_the_value() {
    let huge_level_no_churn = churn(36_089, Some((36_089, 60)));
    let tiny_level_high_churn = churn(4, Some((0, 5)));

    assert!(tiny_level_high_churn.current < huge_level_no_churn.current);
    assert!(!huge_level_no_churn.verdict.is_raise());
    assert!(tiny_level_high_churn.verdict.is_raise());
}

/// Churn under the ceiling is ordinary supervision, not a fault.
#[test]
fn a_delta_within_the_ceiling_does_not_raise() {
    let report = churn(36_092, Some((36_089, 60)));

    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert_eq!(report.delta, Some(3));
}

/// A first observation cannot answer a rate question. It is not a pass, and it
/// is not a restart fault either — it is a named blind spot.
#[test]
fn negative_control_a_missing_baseline_is_unknown_with_a_named_boundary() {
    let report = churn(36_089, None);

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_ne!(report.verdict, ServiceVerdict::Served);
    assert_eq!(report.boundary, Some(Boundary::Scope));
    assert_eq!(report.delta, None);
}

/// A baseline with no interval behind it cannot produce a rate either.
#[test]
fn negative_control_a_zero_length_interval_is_unknown() {
    let report = churn(10, Some((0, 0)));

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Parse));
}

/// A counter that went backwards means the interval spans a reset, so the
/// difference across it is not a restart count.
#[test]
fn negative_control_a_counter_reset_is_unknown_not_a_negative_rate() {
    let report = churn(2, Some((36_089, 60)));

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Parse));
    assert!(report.detail.contains("backwards"));
}

/// Churn is context on an armed unit, but an unmeasurable rate still is not a
/// pass — while a *known* arming fault outranks it, so the reader is sent to
/// the missing unit rather than to the counter.
#[test]
fn a_known_arming_fault_outranks_an_unmeasurable_restart_rate() {
    let mut absent = timer(UnitPresence::Absent, None);
    absent.restarts = Some(RestartObservation {
        current: 36_089,
        baseline: None,
    });
    assert_eq!(assess_unit(&absent).verdict, ServiceVerdict::Unserved);

    let mut armed = armed_timer();
    armed.restarts = Some(RestartObservation {
        current: 36_089,
        baseline: None,
    });
    let report = assess_unit(&armed);
    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Scope));
}

// ---------------------------------------------------------------------------
// Drift — three states, three remedies, and the size of the delta
// ---------------------------------------------------------------------------

fn artifact(
    installed: Option<&str>,
    upstream: Option<UpstreamRecord>,
    delta: Option<LineDelta>,
) -> ArtifactObservation {
    ArtifactObservation {
        name: "tartci-reaper.py".to_owned(),
        installed_path: "/usr/local/bin/tartci-reaper.py".to_owned(),
        installed_digest: installed.map(str::to_owned),
        upstream,
        delta,
    }
}

fn upstream(deployed: Option<&str>, compare: Option<&str>) -> UpstreamRecord {
    UpstreamRecord {
        repo_path: "tools/fleet/tartci-reaper.py".to_owned(),
        deployed_ref: "a04717c".to_owned(),
        deployed_digest: deployed.map(str::to_owned),
        compare_ref: "origin/main".to_owned(),
        compare_digest: compare.map(str::to_owned),
    }
}

/// The positive control for the drift check.
#[test]
fn positive_control_equal_digests_are_in_sync_and_do_not_raise() {
    let report = assess_artifact_drift(&artifact(
        Some("sha256:aaa"),
        Some(upstream(Some("sha256:aaa"), Some("sha256:aaa"))),
        Some(LineDelta::default()),
    ));

    assert_eq!(report.state, DriftState::InSync);
    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert!(!report.verdict.is_raise());
}

/// Untouched on the host, and the repo moved: deploy.
#[test]
fn an_installed_copy_untouched_since_deploy_is_behind_the_repo() {
    let report = assess_artifact_drift(&artifact(
        Some("sha256:deployed"),
        Some(upstream(Some("sha256:deployed"), Some("sha256:newer"))),
        Some(LineDelta {
            added: 0,
            removed: 2,
        }),
    ));

    assert_eq!(report.state, DriftState::BehindRepo);
    assert!(report.verdict.is_raise());
    assert_eq!(report.changed_lines, Some(2));
    assert!(report.detail.contains("deploy the repo copy onto the host"));
}

/// The measured macpro case: 141 + 86 changed lines of host-side work, no
/// symptom, and a redeploy would delete it. This must NOT be the same verdict
/// as "behind", because "behind" means deploy.
#[test]
fn negative_control_an_installed_copy_ahead_of_the_repo_is_not_behind_it() {
    let ahead = assess_artifact_drift(&artifact(
        Some("sha256:host-edited"),
        Some(upstream(Some("sha256:deployed"), Some("sha256:deployed"))),
        Some(LineDelta {
            added: 141,
            removed: 86,
        }),
    ));
    let behind = assess_artifact_drift(&artifact(
        Some("sha256:deployed"),
        Some(upstream(Some("sha256:deployed"), Some("sha256:newer"))),
        Some(LineDelta {
            added: 0,
            removed: 2,
        }),
    ));

    assert_eq!(ahead.state, DriftState::AheadOfRepo);
    assert_ne!(
        ahead.state, behind.state,
        "ahead and behind share one symptom and have opposite remedies; \
         collapsing them is what strips flock off a shared cache"
    );
    assert_ne!(ahead.detail, behind.detail);
    assert!(ahead.verdict.is_raise());
    assert!(ahead.detail.contains("do NOT redeploy"));
    assert!(!behind.detail.contains("do NOT redeploy"));
}

/// Two changed lines and two hundred and twenty-seven are not the same alarm.
#[test]
fn the_delta_size_is_reported_so_two_lines_and_two_hundred_differ() {
    let big = assess_artifact_drift(&artifact(
        Some("sha256:host-edited"),
        Some(upstream(Some("sha256:deployed"), Some("sha256:deployed"))),
        Some(LineDelta {
            added: 141,
            removed: 86,
        }),
    ));
    let small = assess_artifact_drift(&artifact(
        Some("sha256:host-edited"),
        Some(upstream(Some("sha256:deployed"), Some("sha256:deployed"))),
        Some(LineDelta {
            added: 2,
            removed: 0,
        }),
    ));

    assert_eq!(big.changed_lines, Some(227));
    assert_eq!(small.changed_lines, Some(2));
    assert!(big.detail.contains("227 changed line(s)"));
    assert!(small.detail.contains("2 changed line(s)"));
}

/// A host that is ahead while the repo also moved has diverged, and the
/// divergence must be visible rather than inferred.
#[test]
fn a_host_ahead_while_the_repo_advanced_is_reported_as_diverged() {
    let report = assess_artifact_drift(&artifact(
        Some("sha256:host-edited"),
        Some(upstream(Some("sha256:deployed"), Some("sha256:newer"))),
        Some(LineDelta {
            added: 141,
            removed: 86,
        }),
    ));

    assert_eq!(report.state, DriftState::AheadOfRepo);
    assert!(report.repo_advanced);
    assert!(report.detail.contains("diverged"));
    assert!(report.detail.contains("do NOT redeploy"));
}

/// You cannot compare against a ref nobody wrote down.
#[test]
fn negative_control_no_recorded_upstream_ref_is_unknown_with_a_boundary() {
    let report = assess_artifact_drift(&artifact(Some("sha256:aaa"), None, None));

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_ne!(report.verdict, ServiceVerdict::Served);
    assert_eq!(report.state, DriftState::Undetermined);
    assert_eq!(report.boundary, Some(Boundary::Scope));
}

/// Differing digests with no deploy-time digest recorded cannot name a
/// direction, and "differs" alone must never license a redeploy.
#[test]
fn negative_control_an_unrecoverable_direction_is_unknown_not_behind() {
    let report = assess_artifact_drift(&artifact(
        Some("sha256:host"),
        Some(upstream(None, Some("sha256:repo"))),
        Some(LineDelta {
            added: 227,
            removed: 0,
        }),
    ));

    assert_eq!(report.state, DriftState::Undetermined);
    assert_ne!(report.state, DriftState::BehindRepo);
    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Scope));
    assert!(report.detail.contains("do NOT redeploy"));
}

/// An unreadable installed file, and an unreadable repo copy, are each a named
/// blind spot rather than a comparison.
#[test]
fn negative_control_unreadable_sides_are_unknown_with_transport() {
    let unreadable_host = assess_artifact_drift(&artifact(
        None,
        Some(upstream(Some("sha256:aaa"), Some("sha256:aaa"))),
        None,
    ));
    let unreadable_repo = assess_artifact_drift(&artifact(
        Some("sha256:aaa"),
        Some(upstream(None, None)),
        None,
    ));

    assert_eq!(unreadable_host.verdict, ServiceVerdict::Unknown);
    assert_eq!(unreadable_host.boundary, Some(Boundary::Transport));
    assert_eq!(unreadable_repo.verdict, ServiceVerdict::Unknown);
    assert_eq!(unreadable_repo.boundary, Some(Boundary::Transport));
}

// ---------------------------------------------------------------------------
// Roll-up and stable strings
// ---------------------------------------------------------------------------

/// Asserting nothing is not the same as asserting everything passed.
#[test]
fn negative_control_an_empty_roll_up_is_unknown_not_served() {
    assert_eq!(roll_up(&[]), ServiceVerdict::Unknown);
}

#[test]
fn roll_up_takes_the_worst_finding() {
    assert_eq!(
        roll_up(&[
            ServiceVerdict::Served,
            ServiceVerdict::Degraded,
            ServiceVerdict::Served
        ]),
        ServiceVerdict::Degraded
    );
}

#[test]
fn state_strings_are_stable() {
    assert_eq!(UnitKind::Service.as_str(), "service");
    assert_eq!(UnitKind::Timer.as_str(), "timer");
    assert_eq!(ArmingState::NotInstalled.as_str(), "not_installed");
    assert_eq!(ArmingState::NotEnabled.as_str(), "not_enabled");
    assert_eq!(ArmingState::Masked.as_str(), "masked");
    assert_eq!(ArmingState::NoNextElapse.as_str(), "no_next_elapse");
    assert_eq!(ArmingState::Armed.as_str(), "armed");
    assert_eq!(ArmingState::Undetermined.as_str(), "undetermined");
    assert_eq!(DriftState::InSync.as_str(), "in_sync");
    assert_eq!(DriftState::BehindRepo.as_str(), "behind_repo");
    assert_eq!(DriftState::AheadOfRepo.as_str(), "ahead_of_repo");
    assert_eq!(DriftState::Undetermined.as_str(), "undetermined");
    assert_eq!(EnablementState::Enabled.as_str(), "enabled");
    assert_eq!(
        EnablementState::Other("linked".to_owned()).as_str(),
        "linked"
    );
}

/// The three remedies are three different sentences, because they are three
/// different actions.
#[test]
fn each_drift_state_carries_its_own_remedy() {
    assert_ne!(
        DriftState::BehindRepo.remedy(),
        DriftState::AheadOfRepo.remedy()
    );
    assert!(DriftState::AheadOfRepo.remedy().contains("do NOT redeploy"));
    assert!(DriftState::BehindRepo.remedy().contains("deploy"));
    assert_eq!(
        LineDelta {
            added: 141,
            removed: 86
        }
        .changed(),
        227
    );
}
