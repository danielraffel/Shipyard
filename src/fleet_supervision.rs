//! Assert that a lane's service can **survive an ordinary exit**.
//!
//! [`crate::fleet_service`] answers *is this lane being served right now*. This
//! module answers a different question that the same census cannot: *when the
//! thing serving it next exits, will anything bring it back?*
//!
//! The two come apart badly. On 2026-09-05 the only macOS runner for this
//! repository was `online`, `busy`, and working — `Served` by every assertion
//! in `fleet_service`, correctly. A routine force-push then triggered
//! `cancel-in-progress`, the cancel reached the runner as a signal, and it
//! exited:
//!
//! ```text
//! Runner] Received Ctrl-C signal, stop Runner.Listener and Runner.Worker.
//! HostContext] Runner will be shutdown for UserCancelled
//! ```
//!
//! It never came back, because nothing was watching it: the runner was **not**
//! ephemeral, its LaunchAgent declared `RunAtLoad` but no `KeepAlive`, and the
//! job was not loaded in launchd at all. The lane went from `Served` to
//! permanently `Unserved` with no intervening state, and the host stayed up
//! throughout — load 7.29, three *other* runners serving on the same machine.
//!
//! The precondition was **statically visible the entire time**. That is what
//! this module reads. It is the liveness/service distinction moved one step
//! earlier: not "is it up", not even "is it serving", but "is its service
//! survivable".
//!
//! ## The rule this module exists to enforce
//!
//! **An unreadable observation is never a fault.** A LaunchAgent loads into the
//! per-user GUI domain, so `launchctl list` over SSH reports *nothing* for a
//! job that is loaded and running. Three such empty reads in a row look exactly
//! like a finding. Reading them as `Unsupervised` would file a fault against a
//! perfectly supervised runner — so an unknown observation yields
//! [`Restartability::Unknown`] with a [`Boundary`], never a fault.

use crate::fleet_service::Boundary;

/// What a runner's registration says about replacing it.
///
/// Deliberately not a `bool`. "Supervised" and "self-replacing" are different
/// mechanisms with different failure modes, and collapsing "I could not read
/// it" into either is the bug this module is built around.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Restartability {
    /// Something will restart it in place — launchd `KeepAlive`, a supervisor,
    /// a systemd `Restart=`. Names the mechanism so a reader can check it.
    Supervised {
        /// The mechanism that will perform the restart.
        mechanism: String,
    },
    /// The runner is ephemeral by design and a registrar creates a fresh one
    /// per job. Its exit is not a fault; it is the contract.
    SelfReplacing,
    /// Nothing will bring it back. Its next exit — cancel, crash, reboot — is
    /// permanent, and permanent is the operative word: no amount of queued
    /// demand will summon it.
    Unsupervised {
        /// Why nothing will restart it, in a form a human can act on.
        reason: String,
    },
    /// The observation could not be made. **Never** folded into a fault.
    Unknown {
        /// Which boundary stopped the measurement.
        boundary: Boundary,
        /// What specifically could not be read.
        detail: String,
    },
}

impl Restartability {
    /// Whether this is a fault worth raising.
    ///
    /// `Unknown` does **not** raise here, unlike `ServiceVerdict::Unknown`. The
    /// difference is deliberate: an unmeasurable *service* is an outage until
    /// proven otherwise, whereas an unmeasurable *supervision config* is a
    /// latent risk about a lane that is, right now, working. Escalating it
    /// would page a human about a runner serving traffic, and this module's
    /// whole reason to exist is that unreadable is not broken.
    #[must_use]
    pub fn is_fault(&self) -> bool {
        matches!(self, Self::Unsupervised { .. })
    }

    /// Snake-case form for JSON and human output.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Supervised { .. } => "supervised",
            Self::SelfReplacing => "self_replacing",
            Self::Unsupervised { .. } => "unsupervised",
            Self::Unknown { .. } => "unknown",
        }
    }
}

/// What was observed about one runner's registration.
///
/// Every field that can fail to be read is an `Option`, and `None` means
/// *not measured* — never `false`. That distinction is the module.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunnerSupervision {
    /// Runner name as GitHub knows it.
    pub name: String,
    /// Whether the runner is registered `--ephemeral`. `None` when the
    /// registration file could not be read.
    pub ephemeral: Option<bool>,
    /// Whether a registrar exists that creates replacements. Only meaningful
    /// for ephemeral runners.
    pub has_registrar: Option<bool>,
    /// The service label the runner claims to be installed under, if any.
    pub service_label: Option<String>,
    /// Whether that label is loaded in the launchd domain the agent actually
    /// loads into. `None` when the domain could not be queried — which is the
    /// common case over SSH, and must not be read as `false`.
    pub loaded_in_supervisor: Option<bool>,
    /// Whether the service declares a restart-on-exit policy (`KeepAlive`,
    /// `Restart=always`). `None` when the definition could not be read.
    pub restart_on_exit: Option<bool>,
}

/// Decide whether one runner's service survives its own exit.
///
/// Order matters. Every branch that could produce a fault is guarded by an
/// unknown-check first, so a missing observation always exits through
/// [`Restartability::Unknown`] rather than falling through to a fault.
#[must_use]
pub fn assess_restartability(observed: &RunnerSupervision) -> Restartability {
    let Some(ephemeral) = observed.ephemeral else {
        return Restartability::Unknown {
            boundary: Boundary::Parse,
            detail:
                "the runner registration could not be read, so whether it is ephemeral is unknown"
                    .to_owned(),
        };
    };

    if ephemeral {
        return match observed.has_registrar {
            Some(true) => Restartability::SelfReplacing,
            Some(false) => Restartability::Unsupervised {
                reason: "registered ephemeral, so it exits after one job, and no registrar was found to create a replacement".to_owned(),
            },
            None => Restartability::Unknown {
                boundary: Boundary::Scope,
                detail: "registered ephemeral, but whether a registrar replaces it was not observed".to_owned(),
            },
        };
    }

    // A persistent runner. It survives only if something restarts it in place.
    let Some(label) = observed.service_label.as_ref() else {
        return Restartability::Unsupervised {
            reason: "persistent runner with no service installed, so it runs only until it exits"
                .to_owned(),
        };
    };

    // The launchd-domain trap: an unqueryable domain reports the same "not
    // found" as a genuinely absent job. Refuse to distinguish what we cannot.
    let Some(loaded) = observed.loaded_in_supervisor else {
        return Restartability::Unknown {
            boundary: Boundary::Scope,
            detail: format!(
                "{label} was not found, but the supervisor domain was not queryable — a per-user agent is invisible from another session, so this cannot be read as absent"
            ),
        };
    };

    if !loaded {
        return Restartability::Unsupervised {
            reason: format!(
                "{label} is installed but not loaded in the supervisor, so nothing is watching the runner"
            ),
        };
    }

    match observed.restart_on_exit {
        Some(true) => Restartability::Supervised {
            mechanism: format!("{label} restarts on exit"),
        },
        Some(false) => Restartability::Unsupervised {
            reason: format!(
                "{label} is loaded but declares no restart-on-exit policy, so an ordinary cancel or crash is permanent"
            ),
        },
        None => Restartability::Unknown {
            boundary: Boundary::Parse,
            detail: format!("{label} is loaded, but its restart policy could not be read"),
        },
    }
}

/// Whether a lane's service can survive an ordinary exit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Survivability {
    /// Every runner that can serve the lane will be replaced or restarted.
    Survivable,
    /// Some runners are unsupervised, but enough survive that the lane
    /// degrades rather than stops.
    Fragile,
    /// **Every** runner able to serve this lane is unsupervised. The next
    /// ordinary exit ends the lane, and nothing will notice.
    SinglePointOfFailure,
    /// Not enough was observed to judge.
    Unknown,
}

impl Survivability {
    /// Whether this should raise to a human.
    #[must_use]
    pub fn should_raise(self) -> bool {
        matches!(self, Self::SinglePointOfFailure | Self::Fragile)
    }

    /// Snake-case form for JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Survivable => "survivable",
            Self::Fragile => "fragile",
            Self::SinglePointOfFailure => "single_point_of_failure",
            Self::Unknown => "unknown",
        }
    }
}

/// One lane's survivability, with the per-runner findings that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurvivabilityReport {
    /// The lane this describes.
    pub lane: String,
    /// The verdict.
    pub verdict: Survivability,
    /// Per-runner findings, in the order supplied.
    pub runners: Vec<(String, Restartability)>,
    /// One line a human can act on.
    pub summary: String,
}

/// Judge a lane from its runners' supervision.
///
/// A lane with **no** runners is [`Survivability::Unknown`], never
/// `SinglePointOfFailure`: an empty census is the signature of a query in the
/// wrong scope, and `fleet_service` already owns the "declared but served by
/// nobody" verdict with the demand evidence needed to make it.
#[must_use]
pub fn assess_lane_survivability(
    lane: &str,
    observed: &[RunnerSupervision],
) -> SurvivabilityReport {
    let runners: Vec<(String, Restartability)> = observed
        .iter()
        .map(|r| (r.name.clone(), assess_restartability(r)))
        .collect();

    if runners.is_empty() {
        return SurvivabilityReport {
            lane: lane.to_owned(),
            verdict: Survivability::Unknown,
            runners,
            summary: format!(
                "{lane}: no runners observed, which is as likely a scope error as an empty pool — not judged here"
            ),
        };
    }

    let faults = runners.iter().filter(|(_, r)| r.is_fault()).count();
    let unknowns = runners
        .iter()
        .filter(|(_, r)| matches!(r, Restartability::Unknown { .. }))
        .count();
    let total = runners.len();

    let (verdict, summary) = if faults == total {
        (
            Survivability::SinglePointOfFailure,
            format!(
                "{lane}: all {total} runner(s) able to serve this lane are unsupervised — the next ordinary exit ends the lane permanently"
            ),
        )
    } else if faults > 0 {
        (
            Survivability::Fragile,
            format!(
                "{lane}: {faults} of {total} runner(s) are unsupervised; the lane survives but loses capacity permanently"
            ),
        )
    } else if unknowns == total {
        (
            Survivability::Unknown,
            format!("{lane}: supervision could not be read for any of the {total} runner(s)"),
        )
    } else {
        (
            Survivability::Survivable,
            format!("{lane}: every runner is supervised or self-replacing"),
        )
    };

    SurvivabilityReport {
        lane: lane.to_owned(),
        verdict,
        runners,
        summary,
    }
}

#[cfg(test)]
mod tests;
