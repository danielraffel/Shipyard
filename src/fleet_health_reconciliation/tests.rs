//! Tests for health-claim reconciliation.
//!
//! Fixtured on tartci #188: a fleet health checker reported a host dead while
//! it was serving a busy runner. Every assertion is paired with a control,
//! because a detector that refutes every claim is worse than none — it would
//! disable the gate entirely while looking like it was working.

use super::*;

fn t(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_757_000_000 + secs, 0).expect("timestamp")
}

fn ceiling() -> Duration {
    Duration::seconds(300)
}

/// The incident: the signal says `critical`, three runners are busy.
fn serving_host() -> ServiceEvidence {
    ServiceEvidence {
        busy_runners: 3,
        online_runners: 3,
        jobs_completed_after_claim: 1,
    }
}

// ---------------------------------------------------------------------------
// tartci #188, and the control that stops the detector firing on everything
// ---------------------------------------------------------------------------

#[test]
fn a_critical_claim_against_a_serving_host_is_contradicted_and_indicts_the_signal() {
    let verdict = reconcile(
        HealthClaim::Level(HostHealthLevel::Critical),
        Some(t(0)),
        serving_host(),
        ceiling(),
        t(10),
    );
    assert!(verdict.indicts_the_signal(), "{verdict:?}");
    assert_eq!(verdict.as_str(), "contradicted");
}

/// **The control that makes the assertion above mean something.** Same
/// `critical` claim, same timing, but nothing is serving. It must stand — a
/// module that refuted every critical claim would silently disable the gate
/// while appearing to work, which is strictly worse than not having it.
#[test]
fn control_a_critical_claim_with_no_service_evidence_stands() {
    let verdict = reconcile(
        HealthClaim::Level(HostHealthLevel::Critical),
        Some(t(0)),
        ServiceEvidence::default(),
        ceiling(),
        t(10),
    );
    assert!(!verdict.indicts_the_signal(), "{verdict:?}");
    assert_eq!(
        verdict,
        Reconciliation::Unsubstantiated {
            claimed: HostHealthLevel::Critical
        }
    );
}

/// A contradicted claim must not block. This is the whole behavioural point:
/// the host keeps taking work and the operator is told the signal is wrong.
#[test]
fn a_contradicted_claim_is_downgraded_from_block_to_warn() {
    let blocked = HostHealthOutcome::Block {
        level: "critical".to_owned(),
        reason: "memory pressure critical".to_owned(),
    };
    let verdict = reconcile(
        HealthClaim::Level(HostHealthLevel::Critical),
        Some(t(0)),
        serving_host(),
        ceiling(),
        t(10),
    );

    match apply(blocked, &verdict) {
        HostHealthOutcome::Warn(message) => {
            assert!(message.contains("contradicted"), "{message}");
            assert!(message.contains("runner(s) busy"), "{message}");
        }
        other => panic!("a contradicted claim must not block: {other:?}"),
    }
}

/// Pairing control for the one above: an unsubstantiated claim still blocks.
/// Without this, "contradicted does not block" would also pass for an `apply`
/// that never blocked anything.
#[test]
fn control_an_unsubstantiated_claim_still_blocks() {
    let blocked = HostHealthOutcome::Block {
        level: "critical".to_owned(),
        reason: "memory pressure critical".to_owned(),
    };
    let verdict = reconcile(
        HealthClaim::Level(HostHealthLevel::Critical),
        Some(t(0)),
        ServiceEvidence::default(),
        ceiling(),
        t(10),
    );
    assert!(matches!(
        apply(blocked, &verdict),
        HostHealthOutcome::Block { .. }
    ));
}

// ---------------------------------------------------------------------------
// Silence refutes nothing, and proves nothing
// ---------------------------------------------------------------------------

/// An idle host serves nothing and is perfectly healthy. Reading quiet as dead
/// would condemn every unused machine in the fleet.
#[test]
fn an_idle_host_with_a_green_claim_is_corroborated_not_a_fault() {
    let verdict = reconcile(
        HealthClaim::Level(HostHealthLevel::Green),
        Some(t(0)),
        ServiceEvidence::default(),
        ceiling(),
        t(10),
    );
    assert_eq!(verdict, Reconciliation::Corroborated);
    assert!(!verdict.indicts_the_signal());
}

#[test]
fn a_warn_claim_is_never_disputed_even_while_serving() {
    let verdict = reconcile(
        HealthClaim::Level(HostHealthLevel::Warn),
        Some(t(0)),
        serving_host(),
        ceiling(),
        t(10),
    );
    assert_eq!(verdict, Reconciliation::Corroborated);
}

/// A registration can outlive the machine behind it — tartci #189, an offline
/// runner still advertising its labels. So being *online* is not proof of life;
/// only work is. Online-but-idle must not refute a critical claim.
#[test]
fn negative_control_online_runners_alone_do_not_refute_a_critical_claim() {
    let registered_but_idle = ServiceEvidence {
        busy_runners: 0,
        online_runners: 8,
        jobs_completed_after_claim: 0,
    };
    assert!(!registered_but_idle.proves_service());

    let verdict = reconcile(
        HealthClaim::Level(HostHealthLevel::Critical),
        Some(t(0)),
        registered_but_idle,
        ceiling(),
        t(10),
    );
    assert!(
        !verdict.indicts_the_signal(),
        "a census entry is not evidence of work: {verdict:?}"
    );
}

/// The mirror control: one busy runner *is* enough, so the rule above is a
/// real distinction rather than a detector that never fires.
#[test]
fn control_a_single_busy_runner_is_enough_to_refute() {
    let one_busy = ServiceEvidence {
        busy_runners: 1,
        online_runners: 1,
        jobs_completed_after_claim: 0,
    };
    assert!(one_busy.proves_service());
    assert!(
        reconcile(
            HealthClaim::Level(HostHealthLevel::Critical),
            Some(t(0)),
            one_busy,
            ceiling(),
            t(10),
        )
        .indicts_the_signal()
    );
}

// ---------------------------------------------------------------------------
// Staleness and absence are not health
// ---------------------------------------------------------------------------

/// A vitals file whose producer died keeps asserting its last value forever,
/// with total confidence. Past the ceiling nobody is answering.
#[test]
fn a_stale_claim_is_unknown_whatever_it_says() {
    for level in [
        HostHealthLevel::Green,
        HostHealthLevel::Warn,
        HostHealthLevel::Critical,
    ] {
        let verdict = reconcile(
            HealthClaim::Level(level),
            Some(t(0)),
            serving_host(),
            ceiling(),
            t(3600),
        );
        assert!(
            matches!(
                verdict,
                Reconciliation::Unknown {
                    boundary: Boundary::Transport,
                    ..
                }
            ),
            "{level:?} -> {verdict:?}"
        );
    }
}

/// Control: the identical claim inside the ceiling is judged normally, so
/// staleness is a real boundary rather than a blanket refusal.
#[test]
fn control_a_fresh_claim_is_judged_rather_than_shrugged_at() {
    let verdict = reconcile(
        HealthClaim::Level(HostHealthLevel::Critical),
        Some(t(0)),
        serving_host(),
        ceiling(),
        t(299),
    );
    assert_eq!(verdict.as_str(), "contradicted");
}

#[test]
fn an_unreadable_signal_is_unknown_and_never_green() {
    let verdict = reconcile(
        HealthClaim::Unreadable,
        Some(t(0)),
        ServiceEvidence::default(),
        ceiling(),
        t(10),
    );
    assert!(matches!(
        verdict,
        Reconciliation::Unknown {
            boundary: Boundary::Parse,
            ..
        }
    ));
    assert_ne!(verdict, Reconciliation::Corroborated);
}

#[test]
fn a_claim_with_no_timestamp_cannot_be_aged_and_is_unknown() {
    let verdict = reconcile(
        HealthClaim::Level(HostHealthLevel::Critical),
        None,
        serving_host(),
        ceiling(),
        t(10),
    );
    assert!(matches!(
        verdict,
        Reconciliation::Unknown {
            boundary: Boundary::Parse,
            ..
        }
    ));
}

/// An unknown reconciliation must leave a non-blocking outcome alone rather
/// than inventing one.
#[test]
fn an_unknown_reconciliation_passes_the_configured_outcome_through() {
    let verdict = reconcile(
        HealthClaim::Unreadable,
        None,
        ServiceEvidence::default(),
        ceiling(),
        t(10),
    );
    assert_eq!(
        apply(HostHealthOutcome::Ok, &verdict),
        HostHealthOutcome::Ok
    );
}
