//! What a process owes when it correctly refuses to act.
//!
//! [`crate::fleet_selfheal`] returns `Escalate` rather than acting when it
//! cannot prove a resource is idle. That is the right half of the contract.
//! This module is the other end: **the obligation a fail-closed exit carries**,
//! which nothing enforced.
//!
//! The motivating incident is a process that exited because it could not prove
//! it had deleted its VM. That refusal was correct and must never be "fixed" by
//! making it proceed. Everything around it was the defect:
//!
//! - it **raised nothing**, so the refusal was silent and no one learned of it;
//! - it left a **failed lease release unresolved**, so a shared resource stayed
//!   held by a process that no longer existed;
//! - and a naive restart policy would have respawned it into the same unproven
//!   state and crash-looped — the `NRestarts=36088` pattern that
//!   [`crate::fleet_guards::assess_restart_churn`] exists to catch.
//!
//! ## Being right does not discharge the duty
//!
//! This is the whole idea, and it is easy to get backwards. A correct refusal
//! is not a completed obligation — it is the *start* of one. The process knows
//! three things nobody else does at that moment: that it stopped, what it was
//! holding, and why it could not finish. If it exits without saying them, that
//! knowledge dies with it and the only remaining evidence is a resource nobody
//! can account for.
//!
//! So a fail-closed exit must satisfy three obligations, and this module names
//! which are unmet rather than reducing them to a boolean:
//!
//! 1. **Raise.** Somewhere that survives the host, because a journal line on a
//!    machine nobody is looking at is not a signal.
//! 2. **Dispose.** Every held resource gets an owner or an explicit release —
//!    a lease still held by a dead process is a leak whatever the exit code was.
//! 3. **Bound the retry.** Repeating an unproven action is not recovery.

use crate::fleet_service::Boundary;

/// A resource the exiting process was holding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeldResource {
    /// What kind of thing it is — `lease`, `ship-state`, `vm-clone`.
    pub kind: String,
    /// Its identifier, so a human can go look at it.
    pub id: String,
    /// Who owns it now. `None` means nobody: the process exited still holding
    /// it, which is the leak.
    pub owner_after_exit: Option<String>,
}

impl HeldResource {
    /// Whether this resource was left with no owner.
    #[must_use]
    pub fn is_orphaned(&self) -> bool {
        self.owner_after_exit.is_none()
    }
}

/// What happens after the exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryPolicy {
    /// Nothing restarts it. Correct when the state is unproven.
    None,
    /// It will be retried a bounded number of times.
    Bounded {
        /// How many attempts remain.
        attempts: u32,
    },
    /// It will be restarted indefinitely. Combined with an unproven state this
    /// is a crash loop, not recovery.
    Unbounded,
}

/// One obligation a fail-closed exit did not meet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Unmet {
    /// Exited without raising anywhere that survives the host.
    Raise,
    /// Left resources with no owner.
    Dispose {
        /// The orphaned resources, in the order supplied.
        orphaned: Vec<HeldResource>,
    },
    /// Will be restarted without bound into a state it could not prove.
    Bound,
}

impl Unmet {
    /// Snake-case form for JSON and human output.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Raise => "raise",
            Self::Dispose { .. } => "dispose",
            Self::Bound => "bound",
        }
    }
}

/// The verdict on an exit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HandbackVerdict {
    /// Every obligation met, or none applied.
    Discharged,
    /// At least one obligation unmet. **A correct refusal reaches this verdict
    /// exactly as readily as an incorrect one** — that is the point.
    Owing,
    /// Not enough was observed to judge.
    Unknown {
        /// Which boundary stopped the measurement.
        boundary: Boundary,
        /// What specifically could not be established.
        detail: String,
    },
}

/// An exit and what it owed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandbackReport {
    /// The verdict.
    pub verdict: HandbackVerdict,
    /// Obligations not met, most consequential first: an unbounded retry into
    /// unproven state does damage on a timer, an orphaned resource is inert
    /// until someone needs it, and a silent exit only costs the time before
    /// someone notices.
    pub unmet: Vec<Unmet>,
    /// One line a human can act on.
    pub summary: String,
}

/// Judge what a fail-closed exit owed.
///
/// `raised` is the escalation reference — an issue, a ticket, anything that
/// outlives the host. `None` means it exited silently.
///
/// A **clean** exit owes nothing: silence is correct when there is nothing to
/// say, and treating every quiet success as a fault would bury the real ones.
#[must_use]
pub fn assess_exit(
    fail_closed: bool,
    raised: Option<&str>,
    held: &[HeldResource],
    retry: RetryPolicy,
) -> HandbackReport {
    if !fail_closed {
        // A clean exit still leaks if it walked away from a resource, but it
        // owes no escalation and no restart bound.
        let orphaned: Vec<HeldResource> =
            held.iter().filter(|r| r.is_orphaned()).cloned().collect();
        if orphaned.is_empty() {
            return HandbackReport {
                verdict: HandbackVerdict::Discharged,
                unmet: Vec::new(),
                summary: "clean exit holding nothing".to_owned(),
            };
        }
        let count = orphaned.len();
        return HandbackReport {
            verdict: HandbackVerdict::Owing,
            unmet: vec![Unmet::Dispose { orphaned }],
            summary: format!("clean exit, but {count} resource(s) were left with no owner"),
        };
    }

    let mut unmet = Vec::new();

    if matches!(retry, RetryPolicy::Unbounded) {
        unmet.push(Unmet::Bound);
    }

    let orphaned: Vec<HeldResource> = held.iter().filter(|r| r.is_orphaned()).cloned().collect();
    if !orphaned.is_empty() {
        unmet.push(Unmet::Dispose { orphaned });
    }

    // Deliberately last in the ordering and deliberately not skipped when the
    // refusal was right. Correctness of the decision says nothing about whether
    // anyone was told.
    if raised.is_none_or(str::is_empty) {
        unmet.push(Unmet::Raise);
    }

    if unmet.is_empty() {
        return HandbackReport {
            verdict: HandbackVerdict::Discharged,
            unmet,
            summary: "refused, raised it, disposed of what it held, and will not retry blindly"
                .to_owned(),
        };
    }

    let names: Vec<&str> = unmet.iter().map(Unmet::as_str).collect();
    HandbackReport {
        verdict: HandbackVerdict::Owing,
        unmet,
        summary: format!(
            "the refusal may have been correct, but it left {} obligation(s) unmet: {}",
            names.len(),
            names.join(", ")
        ),
    }
}

/// Judge an exit whose own outcome could not be read.
///
/// Separate from [`assess_exit`] so an unobservable exit cannot be silently
/// scored as `Discharged` by passing defaults into it — the shape of mistake
/// this whole family of assertions exists to prevent.
#[must_use]
pub fn unobservable_exit(boundary: Boundary, detail: &str) -> HandbackReport {
    HandbackReport {
        verdict: HandbackVerdict::Unknown {
            boundary,
            detail: detail.to_owned(),
        },
        unmet: Vec::new(),
        summary: format!("the exit could not be observed: {detail}"),
    }
}

#[cfg(test)]
mod tests;
