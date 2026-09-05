//! Reconcile a host-health *claim* against evidence that the host is serving.
//!
//! [`crate::host_health`] reads a `host_vitals` signal and can block a ship
//! when it reports `critical`. That is the right behaviour when the signal is
//! right. This module exists because it is sometimes wrong in the one direction
//! that costs the most.
//!
//! The motivating incident (tartci #188) is a fleet health checker reporting a
//! host **dead while it was serving a busy runner**. That is the only failure
//! mode in this family where the system was *confident and wrong* rather than
//! silent, and it is the worst kind: a confidently wrong "dead" verdict removes
//! a working host from rotation, and once an operator has seen one, they stop
//! trusting every other verdict the same system emits.
//!
//! ## The asymmetry that makes this tractable
//!
//! Health signals and service observations are not equally trustworthy, and
//! they fail in opposite directions:
//!
//! - **Service evidence is positive and hard to fake.** A runner that is `busy`
//!   is executing someone's job. A job that completed after the claim was
//!   written finished on that host. Neither can happen on a dead machine, so
//!   they *refute* a `critical` claim outright.
//! - **Absence of service evidence proves nothing.** An idle host serves
//!   nothing and is perfectly healthy. This is the same distinction
//!   `ServiceVerdict` draws between `Idle` and `Unserved`, and getting it
//!   backwards here would condemn every quiet machine in the fleet.
//!
//! So this module only ever *downgrades* a claim's authority; it never
//! manufactures a fault. Evidence can refute `critical`, and silence can refute
//! nothing.
//!
//! ## Staleness is not health
//!
//! A vitals file whose producer died keeps reporting whatever it last wrote,
//! forever, with total confidence. Past a ceiling the claim is not `green` and
//! not `critical` — it is [`Reconciliation::Unknown`], because nobody is
//! answering.

use chrono::{DateTime, Duration, Utc};

use crate::fleet_service::Boundary;
use crate::host_health::{HostHealthLevel, HostHealthOutcome};

/// What the health signal asserted, including the case where it said nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthClaim {
    /// The signal produced a level.
    Level(HostHealthLevel),
    /// The signal was absent or unreadable. Distinct from `green`: nobody said
    /// the host was fine, we just could not ask.
    Unreadable,
}

/// Observations that a host is doing work, gathered independently of the
/// health signal.
///
/// Every field is a count or an instant, never a summary verdict — a summary
/// would let one wrong judgement contaminate the evidence meant to check it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ServiceEvidence {
    /// Runners on this host currently executing a job.
    pub busy_runners: usize,
    /// Runners on this host registered and online.
    pub online_runners: usize,
    /// Jobs this host finished *after* the claim was written. Ordering matters:
    /// a job that completed before the claim says nothing about it.
    pub jobs_completed_after_claim: usize,
}

impl ServiceEvidence {
    /// Whether this evidence proves the host was alive at claim time or later.
    ///
    /// `online_runners` deliberately does **not** count. A registration can
    /// outlive the machine behind it — that is tartci #189, an offline runner
    /// still advertising its labels — so presence in a census is not proof of
    /// life. Only *work* is.
    #[must_use]
    pub fn proves_service(self) -> bool {
        self.busy_runners > 0 || self.jobs_completed_after_claim > 0
    }
}

/// The outcome of checking a claim against evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reconciliation {
    /// Claim and evidence agree, or the claim is benign and nothing disputes
    /// it. The claim stands.
    Corroborated,
    /// The claim says the host cannot serve, and the evidence shows it
    /// serving. **The claim is at fault, not the host.**
    Contradicted {
        /// What the signal said.
        claimed: HostHealthLevel,
        /// Why the evidence refutes it.
        refutation: String,
    },
    /// A blocking claim with no evidence either way. It is honoured — absence
    /// of proof of life is not proof of death, but neither is it grounds to
    /// override a signal built to see things a job count cannot.
    Unsubstantiated {
        /// What the signal said.
        claimed: HostHealthLevel,
    },
    /// No usable claim: unreadable, or so stale that nobody is answering.
    Unknown {
        /// Which boundary stopped the measurement.
        boundary: Boundary,
        /// What specifically could not be established.
        detail: String,
    },
}

impl Reconciliation {
    /// Snake-case form for JSON and human output.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Corroborated => "corroborated",
            Self::Contradicted { .. } => "contradicted",
            Self::Unsubstantiated { .. } => "unsubstantiated",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// Whether the health signal itself should be reported as faulty.
    ///
    /// Only a contradiction indicts the signal. An unreadable or stale signal
    /// is a gap, not a lie, and conflating the two would send an operator to
    /// debug a producer that is merely quiet.
    #[must_use]
    pub fn indicts_the_signal(&self) -> bool {
        matches!(self, Self::Contradicted { .. })
    }
}

/// Check a claim against evidence.
///
/// `claim_written_at` and `now` bound staleness; a claim older than
/// `staleness_ceiling` is [`Reconciliation::Unknown`] regardless of what it
/// says, because a dead producer repeats its last word indefinitely.
#[must_use]
pub fn reconcile(
    claim: HealthClaim,
    claim_written_at: Option<DateTime<Utc>>,
    evidence: ServiceEvidence,
    staleness_ceiling: Duration,
    now: DateTime<Utc>,
) -> Reconciliation {
    let HealthClaim::Level(level) = claim else {
        return Reconciliation::Unknown {
            boundary: Boundary::Parse,
            detail:
                "the host-health signal was absent or unreadable, which is not the same as green"
                    .to_owned(),
        };
    };

    match claim_written_at {
        Some(written) if now.signed_duration_since(written) > staleness_ceiling => {
            return Reconciliation::Unknown {
                boundary: Boundary::Transport,
                detail: format!(
                    "the signal last wrote {}s ago, past the {}s ceiling — a stale file repeats its last word with full confidence",
                    now.signed_duration_since(written).num_seconds(),
                    staleness_ceiling.num_seconds()
                ),
            };
        }
        None => {
            return Reconciliation::Unknown {
                boundary: Boundary::Parse,
                detail: "the signal carried no timestamp, so its age cannot be bounded".to_owned(),
            };
        }
        Some(_) => {}
    }

    // Only a claim that would take the host out of rotation is worth disputing.
    // A green claim needs no evidence, and manufacturing a fault from a quiet
    // host is precisely the error this module refuses to make.
    if level < HostHealthLevel::Critical {
        return Reconciliation::Corroborated;
    }

    if evidence.proves_service() {
        return Reconciliation::Contradicted {
            claimed: level,
            refutation: format!(
                "the host is serving: {} runner(s) busy and {} job(s) completed after the claim was written — work does not happen on a host that cannot serve",
                evidence.busy_runners, evidence.jobs_completed_after_claim
            ),
        };
    }

    Reconciliation::Unsubstantiated { claimed: level }
}

/// Apply a reconciliation to the gate's configured outcome.
///
/// The single behavioural rule: **a contradicted claim never blocks.** It is
/// downgraded to a warning naming the contradiction, so the operator learns the
/// signal is wrong instead of learning that a working host is unusable.
///
/// Everything else passes through unchanged. An unknown claim does not block
/// either, but that is not this function's decision to make — an unknown claim
/// never produced a `Block` upstream in the first place, because
/// `host_health` only blocks on an explicit `critical`.
#[must_use]
pub fn apply(configured: HostHealthOutcome, reconciliation: &Reconciliation) -> HostHealthOutcome {
    match (&configured, reconciliation) {
        (
            HostHealthOutcome::Block { level, reason },
            Reconciliation::Contradicted { refutation, .. },
        ) => HostHealthOutcome::Warn(format!(
            "host-health reported {level} ({reason}), but that claim is contradicted — {refutation}. Proceeding, and the signal needs attention."
        )),
        _ => configured,
    }
}

#[cfg(test)]
mod tests;
