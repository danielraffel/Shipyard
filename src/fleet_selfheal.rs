//! Fleet **bounded self-heal**: decide whether a corrective action is provably
//! safe to take, and never take it.
//!
//! Sibling to [`crate::fleet_service`] and [`crate::fleet_supervisor`], and
//! deliberately the same shape: pure classification, no I/O, no ambient clock,
//! no process spawning. Callers gather the observations and pass them in with
//! an explicit `now`. The verdict taxonomy is reused wholesale —
//! [`ServiceVerdict`] and [`Boundary`] are imported, never re-declared — so a
//! roll-up can take the worst verdict across service, supervisor and self-heal
//! assertions without translating between vocabularies.
//!
//! ## The governing rule
//!
//! > Act only where the action is provably safe and reversible. **Never** touch
//! > anything that can destroy in-flight work: a running release build, a
//! > required-gate VM, or a clone whose provenance cannot be read. **A
//! > self-heal that cannot prove the target is idle must escalate instead of
//! > acting.**
//!
//! ## This module decides; it never acts
//!
//! Every entry point returns a [`SelfHealDecision`]. Performing the action is
//! the caller's job, and only when handed a [`SelfHealDecision::Act`]. That
//! separation is the safety property, and it is why nothing here takes a
//! command runner, a host handle or a VM client: a module that cannot reach the
//! host cannot destroy a VM by mistake, cannot destroy one on a code path
//! nobody reviewed, and can be exhaustively tested without a fleet. The
//! interesting failure here is destructive and irreversible, so the decision is
//! kept somewhere it can be argued about in isolation.
//!
//! ## Destroy, not stop
//!
//! Freeing memory is not enough. When the orphaned clones from the reaper
//! incident were merely *stopped*, the next dispatch still failed:
//!
//! ```text
//! ERROR: no free clone id in 200..202
//! ```
//!
//! The pool allocates by VMID, so an orphan holds its ID whether it is running
//! or not. A stop reclaims RAM and leaves the actual exhaustion in place, which
//! reads as a successful remediation and fixes nothing. So the approved-action
//! type has **no stop variant at all** — [`Action`] can only express a destroy,
//! and a [`ReapMode::Stop`] proposal is refused as an insufficient remedy
//! rather than quietly upgraded. Making the wrong remedy unrepresentable is
//! cheaper than remembering not to choose it.
//!
//! ## Fresh clones are not orphans
//!
//! The live reaper logs its own restraint:
//!
//! ```text
//! SKIP 200 — clone is only 88s old
//! ```
//!
//! A self-heal that touches clones inside their TTL is a wrecking ball: every
//! healthy job starts as a very young clone, so a TTL check is the difference
//! between reaping leftovers and reaping the fleet. Under TTL the decision is
//! [`SelfHealDecision::Nothing`], not a refusal — there is no fault to remedy.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::fleet_service::{Boundary, ServiceVerdict};

/// Load average at or below which a host is treated as carrying no work.
///
/// A live host never reads exactly zero, so "zero load" has to be a ceiling.
/// It is deliberately tiny: this number is one of four facts that together
/// authorise a destroy, and a generous idle threshold is indistinguishable from
/// no threshold on a box that is merely between build steps.
pub const DEFAULT_IDLE_LOAD_CEILING: f64 = 0.05;

/// Consecutive blind supervisor cycles tolerated before a restart is
/// *considered* — never before one is authorised, which additionally requires
/// queued demand and a proof of idleness.
pub const DEFAULT_BLIND_CYCLES_BEFORE_RESTART: usize = 3;

/// Which lane a clone was created for, as read from its provenance.
///
/// The lane is the input to two of the three never-touch cases, which is why an
/// unreadable provenance is itself a never-touch: without it, a release-build
/// VM and a leftover orphan are the same anonymous VMID.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneKind {
    /// A release build. Never touched while its job is not terminal.
    ReleaseBuild,
    /// A VM serving a required merge gate. Never touched at all.
    RequiredGate,
    /// Ordinary fleet work, reapable once idle and past its TTL.
    Ordinary,
}

impl LaneKind {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReleaseBuild => "release_build",
            Self::RequiredGate => "required_gate",
            Self::Ordinary => "ordinary",
        }
    }
}

/// What the owning job is doing, as far as the caller could establish.
///
/// `Unknown` is not folded into `Terminal`. A job whose state could not be read
/// is exactly the case where a destroy is unrecoverable, so it is kept distinct
/// and treated as in-flight.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// The job is still running.
    Active,
    /// The job reached a terminal conclusion.
    Terminal,
    /// The job's state could not be read.
    Unknown,
}

impl JobState {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Terminal => "terminal",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the owning job has provably finished.
    ///
    /// Only [`JobState::Terminal`] qualifies: an unread state is treated as
    /// in-flight, because the cost of being wrong is asymmetric.
    #[must_use]
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Terminal)
    }
}

/// Provenance read off a clone: who created it, for what, and for how long.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CloneProvenance {
    /// Lane the clone was created for.
    pub lane: LaneKind,
    /// State of the job that created it.
    pub job_state: JobState,
    /// Age, in seconds, after which the clone is considered to have outlived
    /// its job.
    pub ttl_secs: i64,
}

/// A clone as observed by the caller, with every unread fact left as `None`.
///
/// `None` is load-bearing throughout: it is the difference between "read, and
/// idle" and "not read", which is the difference between a destroy and an
/// escalation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CloneObservation {
    /// Pool VMID. The scarce resource — an orphan holds it whether it runs or
    /// not, which is why the remedy has to be a destroy.
    pub vmid: u32,
    /// Clone name, for the operator.
    pub name: String,
    /// When the clone was created.
    pub created_at: DateTime<Utc>,
    /// Provenance, or `None` when it could not be read.
    pub provenance: Option<CloneProvenance>,
    /// Boundary that stopped the provenance read, when one did.
    pub provenance_boundary: Option<Boundary>,
    /// Whether a `Runner.Listener` process is alive inside the guest, or `None`
    /// when the guest could not be inspected.
    pub runner_listener_running: Option<bool>,
    /// Guest load average, or `None` when it could not be read.
    pub load_average: Option<f64>,
    /// Whether a runner for this clone is still registered with GitHub, or
    /// `None` when the census could not be read.
    pub registered_runner: Option<bool>,
}

impl CloneObservation {
    /// Age in seconds at `now`.
    #[must_use]
    pub fn age_secs(&self, now: DateTime<Utc>) -> i64 {
        (now - self.created_at).num_seconds()
    }

    /// TTL declared by provenance, or `None` when provenance is unreadable.
    ///
    /// A clone with no readable TTL has no computable expiry, which is a second
    /// independent reason the unreadable case cannot be reaped.
    #[must_use]
    pub fn ttl_secs(&self) -> Option<i64> {
        self.provenance.as_ref().map(|p| p.ttl_secs)
    }

    /// Whether the clone is past its declared TTL at `now`.
    #[must_use]
    pub fn is_past_ttl(&self, now: DateTime<Utc>) -> bool {
        self.ttl_secs().is_some_and(|ttl| self.age_secs(now) >= ttl)
    }

    /// Derive the never-touch inputs from this observation.
    #[must_use]
    pub fn safety(&self) -> Safety {
        let Some(provenance) = self.provenance.as_ref() else {
            return Safety {
                provenance_unreadable: Some(
                    self.provenance_boundary.unwrap_or(Boundary::Transport),
                ),
                release_build_in_flight: false,
                serves_required_gate: false,
            };
        };
        Safety {
            provenance_unreadable: None,
            release_build_in_flight: provenance.lane == LaneKind::ReleaseBuild
                && !provenance.job_state.is_finished(),
            serves_required_gate: provenance.lane == LaneKind::RequiredGate,
        }
    }
}

/// One fact that must be read before a target can be called idle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleFact {
    /// Who created the target, for what job, with what TTL.
    Provenance,
    /// Whether a `Runner.Listener` is alive inside the guest.
    RunnerListener,
    /// Guest load average.
    Load,
    /// Whether a runner is still registered with GitHub for this target.
    RunnerRegistration,
}

impl IdleFact {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provenance => "provenance",
            Self::RunnerListener => "runner_listener",
            Self::Load => "load",
            Self::RunnerRegistration => "runner_registration",
        }
    }
}

/// How idleness was — or was not — established.
///
/// Idle proof is a first-class input rather than an assumption, because the
/// destructive action is authorised by it. The variants below are the ways the
/// question can come back, and only one of them authorises anything: absence,
/// partial reads and unreadable provenance are all [`SelfHealDecision::Escalate`].
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IdleProof {
    /// Every fact was read, and every one says the target carries no work.
    Proven,
    /// Provenance could not be read.
    ///
    /// Named separately from [`IdleProof::Partial`] because it is not merely a
    /// gap in the proof: it is one of the three never-touch cases, and it stays
    /// a refusal no matter what the other three facts say.
    ProvenanceUnreadable {
        /// Boundary that stopped the read.
        boundary: Boundary,
    },
    /// A fact was read and says the target is working.
    Busy {
        /// Which fact contradicted idleness.
        fact: IdleFact,
        /// The reading, for the operator.
        detail: String,
    },
    /// Some facts were read; at least one was not.
    Partial {
        /// Facts that were never read.
        unread: Vec<IdleFact>,
    },
    /// No idleness observation was attempted at all.
    Absent,
}

impl IdleProof {
    /// Snake-case discriminant, for JSON output and grouping.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Proven => "proven",
            Self::ProvenanceUnreadable { .. } => "provenance_unreadable",
            Self::Busy { .. } => "busy",
            Self::Partial { .. } => "partial",
            Self::Absent => "absent",
        }
    }

    /// Whether this proof authorises a destructive action.
    ///
    /// True for [`IdleProof::Proven`] alone. Every other variant is a question
    /// that did not come back answered, and an unanswered question is not a
    /// yes.
    #[must_use]
    pub fn authorises_action(&self) -> bool {
        matches!(self, Self::Proven)
    }

    /// Operator-facing account of what was and was not established.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Proven => "provenance readable, no Runner.Listener in the guest, load at rest, \
                             and no runner still registered"
                .to_owned(),
            Self::ProvenanceUnreadable { boundary } => format!(
                "provenance could not be read ({}) — {}",
                boundary.as_str(),
                boundary.next_action()
            ),
            Self::Busy { fact, detail } => {
                format!("`{}` says the target is working: {detail}", fact.as_str())
            }
            Self::Partial { unread } => format!(
                "never read: [{}]",
                unread
                    .iter()
                    .map(|fact| fact.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::Absent => "no idleness observation was attempted".to_owned(),
        }
    }
}

/// Establish whether an observed clone is provably idle.
///
/// The four facts are read in order of decisiveness. A single fact that says
/// *working* settles the question regardless of how many others are missing;
/// only after none contradicts idleness does a missing fact become the answer.
#[must_use]
pub fn prove_clone_idle(observation: &CloneObservation) -> IdleProof {
    if observation.provenance.is_none() {
        return IdleProof::ProvenanceUnreadable {
            boundary: observation
                .provenance_boundary
                .unwrap_or(Boundary::Transport),
        };
    }

    if observation.runner_listener_running == Some(true) {
        return IdleProof::Busy {
            fact: IdleFact::RunnerListener,
            detail: "a Runner.Listener process is alive inside the guest".to_owned(),
        };
    }
    if let Some(load) = observation.load_average
        && load > DEFAULT_IDLE_LOAD_CEILING
    {
        return IdleProof::Busy {
            fact: IdleFact::Load,
            detail: format!(
                "load average {load:.2} exceeds the {DEFAULT_IDLE_LOAD_CEILING:.2} idle ceiling"
            ),
        };
    }
    if observation.registered_runner == Some(true) {
        return IdleProof::Busy {
            fact: IdleFact::RunnerRegistration,
            detail: "a runner for this clone is still registered with GitHub".to_owned(),
        };
    }

    let mut unread = Vec::new();
    if observation.runner_listener_running.is_none() {
        unread.push(IdleFact::RunnerListener);
    }
    if observation.load_average.is_none() {
        unread.push(IdleFact::Load);
    }
    if observation.registered_runner.is_none() {
        unread.push(IdleFact::RunnerRegistration);
    }

    if unread.is_empty() {
        IdleProof::Proven
    } else {
        IdleProof::Partial { unread }
    }
}

/// The three targets that are never touched, whatever else is true of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NeverTouch {
    /// A release build whose job has not reported terminal.
    ///
    /// Idleness is not decidable for one of these: a release build parked
    /// waiting on notarization has no listener, no load and no registered
    /// runner, which is byte-for-byte the reading an orphan gives.
    RunningReleaseBuild,
    /// A VM serving a required merge gate.
    ///
    /// Refused unconditionally, terminal job or not. The blast radius of being
    /// wrong is every open PR, and the reaper is never the right actor to take
    /// that risk on its own authority.
    RequiredGateVm,
    /// A target whose provenance could not be read.
    ///
    /// An anonymous VMID cannot be told apart from either case above. Folding
    /// an unreadable instrument into "safe to destroy" is the failure this
    /// module exists to end.
    UnreadableProvenance {
        /// Boundary that stopped the read.
        boundary: Boundary,
    },
}

impl NeverTouch {
    /// Snake-case discriminant, for JSON output and grouping.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::RunningReleaseBuild => "running_release_build",
            Self::RequiredGateVm => "required_gate_vm",
            Self::UnreadableProvenance { .. } => "unreadable_provenance",
        }
    }

    /// Operator-facing account of the refusal.
    #[must_use]
    pub fn detail(self) -> String {
        match self {
            Self::RunningReleaseBuild => {
                "provenance names a release build whose job has not reported terminal — a release \
                 build between phases reads exactly as idle as an orphan does"
                    .to_owned()
            }
            Self::RequiredGateVm => {
                "provenance names a VM serving a required merge gate — refused unconditionally, \
                 because being wrong here blocks every open PR"
                    .to_owned()
            }
            Self::UnreadableProvenance { boundary } => format!(
                "provenance could not be read ({}), so this is an anonymous VMID that cannot be \
                 distinguished from a release build or a gate VM — {}",
                boundary.as_str(),
                boundary.next_action()
            ),
        }
    }

    /// Boundary carried by this refusal, when it is a measurement failure.
    #[must_use]
    pub fn boundary(self) -> Option<Boundary> {
        match self {
            Self::UnreadableProvenance { boundary } => Some(boundary),
            Self::RunningReleaseBuild | Self::RequiredGateVm => None,
        }
    }
}

/// Never-touch inputs, derived per target and checked before anything else.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Safety {
    /// `Some(boundary)` when the target's provenance could not be read.
    pub provenance_unreadable: Option<Boundary>,
    /// The target is, or hosts, a release build that has not reported terminal.
    pub release_build_in_flight: bool,
    /// The target serves a required merge gate.
    pub serves_required_gate: bool,
}

impl Safety {
    /// Inputs for a target with nothing special about it.
    #[must_use]
    pub fn ordinary() -> Self {
        Self {
            provenance_unreadable: None,
            release_build_in_flight: false,
            serves_required_gate: false,
        }
    }
}

/// Which never-touch case a target falls into, if any.
///
/// Checked before the fault preconditions and before the idle proof, and
/// overridden by neither. A proposal aimed at one of these is a defect in the
/// caller's target selection, and it is reported as an escalation even when the
/// fault preconditions are not met — swallowing it as
/// [`SelfHealDecision::Nothing`] because the TTL happened not to have elapsed
/// hides the defect until the day it has.
#[must_use]
pub fn never_touch(safety: &Safety) -> Option<NeverTouch> {
    if let Some(boundary) = safety.provenance_unreadable {
        return Some(NeverTouch::UnreadableProvenance { boundary });
    }
    if safety.release_build_in_flight {
        return Some(NeverTouch::RunningReleaseBuild);
    }
    if safety.serves_required_gate {
        return Some(NeverTouch::RequiredGateVm);
    }
    None
}

/// How a caller proposes to reclaim a clone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReapMode {
    /// Shut the guest down, leaving the clone defined.
    Stop,
    /// Remove the clone and release its VMID.
    Destroy,
}

impl ReapMode {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Destroy => "destroy",
        }
    }

    /// Whether this mode releases the clone's VMID.
    ///
    /// The pool allocates by VMID, so this — not memory — is what decides
    /// whether the remedy addresses the fault.
    #[must_use]
    pub fn frees_vmid(self) -> bool {
        matches!(self, Self::Destroy)
    }
}

/// A change to the relay chain.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelayChange {
    /// Remove a hop from the chain.
    DropHop {
        /// Hop name.
        hop: String,
    },
    /// Re-order the existing hops without removing any.
    Reorder {
        /// The proposed order, which must be a permutation of the current hops.
        order: Vec<String>,
    },
}

/// One hop in the relay chain.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RelayHop {
    /// Hop name.
    pub name: String,
    /// Whether the hop answered at all.
    pub reachable: bool,
    /// Connect attempts that failed.
    pub connect_failures: u32,
    /// Failures the hop is allowed before it is considered spent. Zero means
    /// no budget is declared, and a hop with no budget can never be over it.
    pub connect_budget: u32,
}

impl RelayHop {
    /// Whether the hop has spent its connect budget.
    #[must_use]
    pub fn over_budget(&self) -> bool {
        self.connect_budget > 0 && self.connect_failures >= self.connect_budget
    }

    /// Whether the hop is currently carrying traffic successfully.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.reachable && !self.over_budget()
    }
}

/// The relay chain as observed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RelayTopology {
    /// Hops, in current order.
    pub hops: Vec<RelayHop>,
}

impl RelayTopology {
    /// Healthy hops remaining if `dropped` were removed.
    #[must_use]
    pub fn healthy_without(&self, dropped: &str) -> usize {
        self.hops
            .iter()
            .filter(|hop| hop.name != dropped && hop.is_healthy())
            .count()
    }

    /// Whether `order` names exactly the current hops, once each.
    #[must_use]
    pub fn is_permutation(&self, order: &[String]) -> bool {
        if order.len() != self.hops.len() {
            return false;
        }
        let mut proposed: Vec<&str> = order.iter().map(String::as_str).collect();
        let mut current: Vec<&str> = self.hops.iter().map(|hop| hop.name.as_str()).collect();
        proposed.sort_unstable();
        current.sort_unstable();
        proposed == current
    }
}

/// A supervisor as observed, for the restart decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SupervisorObservation {
    /// Lane the supervisor serves.
    pub lane: String,
    /// Consecutive cycles in which it could not read the queue.
    pub consecutive_blind_cycles: usize,
    /// Consecutive blind cycles tolerated before a restart is considered.
    pub blind_cycles_before_restart: usize,
    /// Jobs queued on this supervisor's labels.
    pub queued_demand: usize,
}

impl SupervisorObservation {
    /// Whether the supervisor has been blind long enough to consider a restart.
    #[must_use]
    pub fn is_blind_past_threshold(&self) -> bool {
        self.consecutive_blind_cycles >= self.blind_cycles_before_restart
    }
}

/// What the caller proposed doing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Proposal {
    /// Reclaim a clone.
    ReapClone {
        /// Pool VMID.
        vmid: u32,
        /// How the caller proposed to reclaim it.
        mode: ReapMode,
    },
    /// Restart a supervisor.
    RestartSupervisor {
        /// Lane the supervisor serves.
        lane: String,
    },
    /// Change the relay chain.
    RelayChange(RelayChange),
}

/// An action this module has authorised.
///
/// There is deliberately **no stop variant**. Stopping an orphaned clone leaves
/// its VMID allocated and the pool still exhausted, so it is not a remedy for
/// the only clone fault this module recognises, and making it unrepresentable
/// means no future caller can pick it by accident.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Destroy the clone, releasing its VMID.
    DestroyClone {
        /// Pool VMID to release.
        vmid: u32,
        /// Clone name.
        name: String,
    },
    /// Restart the supervisor serving `lane`.
    RestartSupervisor {
        /// Lane the supervisor serves.
        lane: String,
    },
    /// Remove `hop` from the relay chain.
    DropRelayHop {
        /// Hop to remove.
        hop: String,
    },
    /// Re-order the relay chain, removing nothing.
    ReorderRelayHops {
        /// The new order.
        order: Vec<String>,
    },
}

impl Action {
    /// Snake-case discriminant, for JSON output and grouping.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DestroyClone { .. } => "destroy_clone",
            Self::RestartSupervisor { .. } => "restart_supervisor",
            Self::DropRelayHop { .. } => "drop_relay_hop",
            Self::ReorderRelayHops { .. } => "reorder_relay_hops",
        }
    }
}

/// An action, together with the proof that authorised it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ApprovedAction {
    /// The action the caller may perform.
    pub action: Action,
    /// When the authorisation was decided.
    pub decided_at: DateTime<Utc>,
    /// What was established, in the order it was established.
    pub justification: String,
}

/// Why an action was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Refusal {
    /// The target is on the never-touch list.
    NeverTouch(NeverTouch),
    /// Idleness was not established.
    IdleUnproven,
    /// The proposed remedy would not fix the fault.
    InsufficientRemedy,
    /// The change would leave the relay with no healthy hop.
    WouldSeverRelay,
    /// The proposal names something the observation does not contain.
    UnknownTarget,
}

impl Refusal {
    /// Snake-case discriminant, for JSON output and grouping.
    #[must_use]
    pub fn kind(self) -> &'static str {
        match self {
            Self::NeverTouch(_) => "never_touch",
            Self::IdleUnproven => "idle_unproven",
            Self::InsufficientRemedy => "insufficient_remedy",
            Self::WouldSeverRelay => "would_sever_relay",
            Self::UnknownTarget => "unknown_target",
        }
    }

    /// What a human should do about it, phrased as an action.
    #[must_use]
    pub fn next_action(self) -> &'static str {
        match self {
            Self::NeverTouch(NeverTouch::RunningReleaseBuild) => {
                "confirm the release build has finished, then reap by hand; no automated remedy \
                 will act on a release build"
            }
            Self::NeverTouch(NeverTouch::RequiredGateVm) => {
                "a gate VM is only ever recycled deliberately — verify no PR is depending on it \
                 and act by hand"
            }
            Self::NeverTouch(NeverTouch::UnreadableProvenance { .. }) => {
                "restore the provenance read before reaping anything in this pool; until it is \
                 readable every VMID here is anonymous and none of them can be reaped safely"
            }
            Self::IdleUnproven => {
                "establish the missing idleness facts and re-decide; do not act on a partial proof"
            }
            Self::InsufficientRemedy => {
                "re-propose the remedy that actually frees the exhausted resource — for a clone \
                 that is a destroy, because a stopped clone still holds its VMID"
            }
            Self::WouldSeverRelay => {
                "repair or add a hop first; the relay must retain a healthy hop through any change"
            }
            Self::UnknownTarget => {
                "re-read the topology and re-decide; the proposal names something the current \
                 observation does not contain"
            }
        }
    }
}

/// A refused proposal, carrying enough for the escalation surface to render a
/// body without re-deriving anything.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Escalation {
    /// What was proposed.
    pub proposed: Proposal,
    /// Why it was refused.
    pub refusal: Refusal,
    /// Severity, in the shared taxonomy. [`ServiceVerdict::Unknown`] whenever
    /// the refusal came from an instrument that could not read, so a refusal
    /// born of blindness never rolls up as a healthy-but-declined action.
    pub verdict: ServiceVerdict,
    /// Boundary that stopped the measurement, when one did.
    pub boundary: Option<Boundary>,
    /// When the refusal was decided.
    pub decided_at: DateTime<Utc>,
    /// Why it was refused, in prose.
    pub detail: String,
    /// What a human should do.
    pub human_action: String,
}

impl Escalation {
    /// Build an escalation, deriving severity and boundary from the refusal.
    #[must_use]
    pub fn new(proposed: Proposal, refusal: Refusal, detail: String, now: DateTime<Utc>) -> Self {
        let boundary = match refusal {
            Refusal::NeverTouch(case) => case.boundary(),
            Refusal::IdleUnproven
            | Refusal::InsufficientRemedy
            | Refusal::WouldSeverRelay
            | Refusal::UnknownTarget => None,
        };
        let verdict = if boundary.is_some() {
            ServiceVerdict::Unknown
        } else {
            ServiceVerdict::Degraded
        };
        Self {
            proposed,
            refusal,
            verdict,
            boundary,
            decided_at: now,
            detail,
            human_action: refusal.next_action().to_owned(),
        }
    }
}

/// What the caller may do.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum SelfHealDecision {
    /// There is no fault to remedy. Not a refusal — a clone inside its TTL, or
    /// a blind supervisor nobody is waiting on, is a healthy steady state.
    Nothing {
        /// Why nothing is warranted.
        reason: String,
    },
    /// The action is authorised. The caller performs it; this module does not.
    Act(ApprovedAction),
    /// The action was refused and a human is needed.
    Escalate(Escalation),
}

impl SelfHealDecision {
    /// Snake-case discriminant, for JSON output and grouping.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Nothing { .. } => "nothing",
            Self::Act(_) => "act",
            Self::Escalate(_) => "escalate",
        }
    }

    /// The authorised action, when there is one.
    #[must_use]
    pub fn action(&self) -> Option<&Action> {
        match self {
            Self::Act(approved) => Some(&approved.action),
            Self::Nothing { .. } | Self::Escalate(_) => None,
        }
    }

    /// The escalation, when the proposal was refused.
    #[must_use]
    pub fn escalation(&self) -> Option<&Escalation> {
        match self {
            Self::Escalate(escalation) => Some(escalation),
            Self::Nothing { .. } | Self::Act(_) => None,
        }
    }

    /// Whether this decision authorises the caller to touch the host.
    #[must_use]
    pub fn is_act(&self) -> bool {
        matches!(self, Self::Act(_))
    }
}

/// Refuse a proposal because the target is on the never-touch list.
fn refuse_never_touch(proposed: Proposal, case: NeverTouch, now: DateTime<Utc>) -> Escalation {
    Escalation::new(
        proposed,
        Refusal::NeverTouch(case),
        format!(
            "refused before any precondition was considered: {}. No idle proof overrides this.",
            case.detail()
        ),
        now,
    )
}

/// Refuse a proposal because idleness was not established.
///
/// An unreadable provenance arriving through the proof rather than through
/// [`Safety`] is re-routed to the never-touch refusal, so that case reads
/// identically whichever path found it.
fn refuse_unproven_idle(proposed: Proposal, proof: &IdleProof, now: DateTime<Utc>) -> Escalation {
    if let IdleProof::ProvenanceUnreadable { boundary } = proof {
        return refuse_never_touch(
            proposed,
            NeverTouch::UnreadableProvenance {
                boundary: *boundary,
            },
            now,
        );
    }
    Escalation::new(
        proposed,
        Refusal::IdleUnproven,
        format!(
            "every precondition for this remedy holds, but idleness is `{}`: {}. A self-heal that \
             cannot prove the target is idle escalates instead of acting.",
            proof.kind(),
            proof.detail()
        ),
        now,
    )
}

/// Decide whether an orphaned clone may be reclaimed, and how.
///
/// Checks run in this order, and the order is the design:
///
/// 1. **Never-touch**, which nothing overrides;
/// 2. **TTL**, which decides whether there is a fault at all;
/// 3. **Remedy sufficiency** — a stop does not free the VMID, so it is not a
///    remedy for pool exhaustion however tidy it looks;
/// 4. **Idle proof**, which is what actually authorises the destroy.
#[must_use]
pub fn decide_clone_reap(
    observation: &CloneObservation,
    mode: ReapMode,
    now: DateTime<Utc>,
) -> SelfHealDecision {
    let proposed = Proposal::ReapClone {
        vmid: observation.vmid,
        mode,
    };

    if let Some(case) = never_touch(&observation.safety()) {
        return SelfHealDecision::Escalate(refuse_never_touch(proposed, case, now));
    }

    let age = observation.age_secs(now);
    if !observation.is_past_ttl(now) {
        let ttl = observation.ttl_secs().unwrap_or_default();
        return SelfHealDecision::Nothing {
            reason: format!(
                "SKIP {} — clone is only {age}s old, inside its {ttl}s TTL. Every healthy job \
                 starts as a young clone, so this is a steady state, not a fault.",
                observation.vmid
            ),
        };
    }

    if !mode.frees_vmid() {
        return SelfHealDecision::Escalate(Escalation::new(
            proposed,
            Refusal::InsufficientRemedy,
            format!(
                "a `{}` leaves clone {} defined and its VMID allocated. The pool allocates by \
                 VMID, so an orphan holds its ID whether it is running or not, and the next \
                 dispatch still fails with `no free clone id`. Freeing memory is not the fault.",
                mode.as_str(),
                observation.vmid
            ),
            now,
        ));
    }

    let proof = prove_clone_idle(observation);
    if !proof.authorises_action() {
        return SelfHealDecision::Escalate(refuse_unproven_idle(proposed, &proof, now));
    }

    SelfHealDecision::Act(ApprovedAction {
        action: Action::DestroyClone {
            vmid: observation.vmid,
            name: observation.name.clone(),
        },
        decided_at: now,
        justification: format!(
            "clone {} is {age}s old against a {}s TTL, is not a release build or a gate VM, and \
             is provably idle ({}). Destroy — not stop — because the VMID is the exhausted \
             resource.",
            observation.vmid,
            observation.ttl_secs().unwrap_or_default(),
            proof.detail()
        ),
    })
}

/// Decide whether a blind supervisor may be restarted.
///
/// Both conditions are required: blind past its threshold **and** demand queued
/// on its labels. A blind supervisor nobody is waiting on is not urgent, and
/// restarting it is unjustified churn against a component whose restart is
/// itself how half-created clones are orphaned.
///
/// `idle` is supplied by the caller rather than derived, because a supervisor's
/// idleness is a different observation from a clone's: what must be proven here
/// is that it is not mid-boot. Restarting a supervisor between "clone created"
/// and "runner registered" produces exactly the orphan this module's other
/// self-heal exists to clean up.
#[must_use]
pub fn decide_supervisor_restart(
    observation: &SupervisorObservation,
    safety: &Safety,
    idle: &IdleProof,
    now: DateTime<Utc>,
) -> SelfHealDecision {
    let proposed = Proposal::RestartSupervisor {
        lane: observation.lane.clone(),
    };

    if let Some(case) = never_touch(safety) {
        return SelfHealDecision::Escalate(refuse_never_touch(proposed, case, now));
    }

    if !observation.is_blind_past_threshold() {
        return SelfHealDecision::Nothing {
            reason: format!(
                "supervisor `{}` is {} consecutive cycles blind, under its {} threshold",
                observation.lane,
                observation.consecutive_blind_cycles,
                observation.blind_cycles_before_restart
            ),
        };
    }

    if observation.queued_demand == 0 {
        return SelfHealDecision::Nothing {
            reason: format!(
                "supervisor `{}` is {} consecutive cycles blind, but nothing is queued on its \
                 labels. Blindness with no demand costs nothing yet, and a restart is churn \
                 against the component whose restart orphans half-created clones.",
                observation.lane, observation.consecutive_blind_cycles
            ),
        };
    }

    if !idle.authorises_action() {
        return SelfHealDecision::Escalate(refuse_unproven_idle(proposed, idle, now));
    }

    SelfHealDecision::Act(ApprovedAction {
        action: Action::RestartSupervisor {
            lane: observation.lane.clone(),
        },
        decided_at: now,
        justification: format!(
            "supervisor `{}` has been blind for {} consecutive cycles (threshold {}) while {} \
             job(s) wait on its labels, and it is provably not mid-dispatch ({})",
            observation.lane,
            observation.consecutive_blind_cycles,
            observation.blind_cycles_before_restart,
            observation.queued_demand,
            idle.detail()
        ),
    })
}

/// Decide whether a relay hop may be dropped or the chain re-ordered.
///
/// The chain must survive the change: a remedy that leaves no healthy hop has
/// severed the relay, which is a worse outage than the failing hop it was
/// aimed at.
#[must_use]
pub fn decide_relay_change(
    topology: &RelayTopology,
    change: &RelayChange,
    safety: &Safety,
    idle: &IdleProof,
    now: DateTime<Utc>,
) -> SelfHealDecision {
    let proposed = Proposal::RelayChange(change.clone());

    if let Some(case) = never_touch(safety) {
        return SelfHealDecision::Escalate(refuse_never_touch(proposed, case, now));
    }

    let structural = match change {
        RelayChange::DropHop { hop } => check_hop_drop(topology, hop),
        RelayChange::Reorder { order } => check_hop_reorder(topology, order),
    };
    let decided = match structural {
        Ok(Some(action)) => action,
        Ok(None) => {
            return SelfHealDecision::Nothing {
                reason: relay_no_fault_reason(topology, change),
            };
        }
        Err((refusal, detail)) => {
            return SelfHealDecision::Escalate(Escalation::new(proposed, refusal, detail, now));
        }
    };

    if !idle.authorises_action() {
        return SelfHealDecision::Escalate(refuse_unproven_idle(proposed, idle, now));
    }

    SelfHealDecision::Act(ApprovedAction {
        action: decided,
        decided_at: now,
        justification: format!(
            "the change leaves {} healthy hop(s) carrying the relay, and no transfer is in flight \
             through it ({})",
            surviving_healthy(topology, change),
            idle.detail()
        ),
    })
}

/// Structural check for a hop drop: known, spent, and survivable.
fn check_hop_drop(
    topology: &RelayTopology,
    hop: &str,
) -> Result<Option<Action>, (Refusal, String)> {
    let Some(target) = topology.hops.iter().find(|candidate| candidate.name == hop) else {
        return Err((
            Refusal::UnknownTarget,
            format!(
                "hop `{hop}` is not in the observed chain [{}]",
                hop_names(topology)
            ),
        ));
    };

    if !target.over_budget() {
        return Ok(None);
    }

    let surviving = topology.healthy_without(hop);
    if surviving == 0 {
        return Err((
            Refusal::WouldSeverRelay,
            format!(
                "dropping `{hop}` would leave 0 healthy hops in [{}] — the relay would be severed, \
                 which is a worse outage than the failing hop it was aimed at",
                hop_names(topology)
            ),
        ));
    }

    Ok(Some(Action::DropRelayHop {
        hop: hop.to_owned(),
    }))
}

/// Structural check for a re-order: a permutation, over something healthy.
fn check_hop_reorder(
    topology: &RelayTopology,
    order: &[String],
) -> Result<Option<Action>, (Refusal, String)> {
    if !topology.is_permutation(order) {
        return Err((
            Refusal::UnknownTarget,
            format!(
                "proposed order [{}] is not a permutation of the observed chain [{}]",
                order.join(","),
                hop_names(topology)
            ),
        ));
    }

    if !topology.hops.iter().any(RelayHop::over_budget) {
        return Ok(None);
    }

    if !topology.hops.iter().any(RelayHop::is_healthy) {
        return Err((
            Refusal::WouldSeverRelay,
            format!(
                "no hop in [{}] is healthy, so re-ordering promotes nothing — the relay is already \
                 severed and needs a hop repaired, not re-arranged",
                hop_names(topology)
            ),
        ));
    }

    Ok(Some(Action::ReorderRelayHops {
        order: order.to_vec(),
    }))
}

/// Why a relay change is unwarranted rather than refused.
fn relay_no_fault_reason(topology: &RelayTopology, change: &RelayChange) -> String {
    match change {
        RelayChange::DropHop { hop } => {
            format!("hop `{hop}` is inside its connect budget — there is no fault to remedy")
        }
        RelayChange::Reorder { .. } => format!(
            "no hop in [{}] has spent its connect budget — there is no fault to remedy",
            hop_names(topology)
        ),
    }
}

/// Healthy hops the chain would retain after `change`.
fn surviving_healthy(topology: &RelayTopology, change: &RelayChange) -> usize {
    match change {
        RelayChange::DropHop { hop } => topology.healthy_without(hop),
        RelayChange::Reorder { .. } => topology.hops.iter().filter(|hop| hop.is_healthy()).count(),
    }
}

/// Comma-joined hop names, for operator-facing messages.
fn hop_names(topology: &RelayTopology) -> String {
    topology
        .hops
        .iter()
        .map(|hop| hop.name.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests;
