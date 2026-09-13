//! Landability: can this PR's **required** contexts actually be scheduled?
//!
//! Shipyard models its own validation. It does not, today, model whether the
//! pull request can merge. On 2026-09-13 that gap cost roughly six hours in
//! which no PR in the repository could land: `PULP_PREAMBLE_RUNS_ON_JSON`
//! named the label `pulp-preamble`, no runner in either registration scope
//! carried it, and every `build.yml` run queued forever at its first job while
//! `shipyard status` reported `mac: local reachable=true` and `ship-state list`
//! reported the blocked PR as healthy. Every word of both was true.
//!
//! The missing question is not "is validation green" but:
//!
//! > can the required contexts on this pull request be **scheduled** onto a
//! > runner that exists?
//!
//! That is answerable before a single job queues, cheaply, and it is what this
//! module answers.
//!
//! ## Why this is not a runner-label lookup
//!
//! The obvious framing — "resolve `runs-on` and check the label against the
//! census" — is one quarter of the check. A required context is produced by a
//! job, that job has `needs`, and the whole transitive closure has to be
//! schedulable for the context to appear at all. On the fleet this was written
//! against, the required `macos` context is produced by an alias job on
//! `PULP_ALIAS_RUNS_ON_JSON` *or* by a matrix leg whose `runs-on` comes from a
//! job output, and **every** path passes through two preamble jobs on
//! `PULP_PREAMBLE_RUNS_ON_JSON`. Four lanes gate one context. Checking the
//! context's own job would have found nothing wrong.
//!
//! ## Why an empty census cannot decide this alone
//!
//! The required macOS leg is served by just-in-time runners that register only
//! while a job is in flight. An idle census is legitimately empty, so a check
//! that refused on "zero runners carry these labels" would refuse on every
//! ship and be disabled within a week — the alarm-fatigue cousin of the retry
//! storm this tool must not become.
//!
//! [`crate::fleet_service::assess_lane_service`] already encodes the rule that
//! resolves this: `Unserved` requires *demand* that has aged past a threshold.
//! At preflight there is no demand yet — the whole point is to speak before the
//! work queues — so the missing input is supplied from the host side instead:
//! a **host attestation** (written by tartci, see [`attestation`]) saying
//! whether any host on this fleet declares and supervises the lane. A lane no
//! host declares and no runner serves is unschedulable now and will be
//! unschedulable in an hour.
//!
//! ## What it never does
//!
//! It never dispatches, re-dispatches, cancels or retries. The repository's own
//! decisions contract, row `[default] #4`, is explicit: *a runnerless required
//! lane is HELD, never a retry storm.* The output of this module is a loud,
//! specific diagnosis naming the context, the job, the variable, the census in
//! both scopes and the per-host attestation — and the two remedies, neither of
//! which it pulls.
//!
//! ## Failing open vs failing closed
//!
//! The refusal is gated on the instrument being *alive*: a lane resolves to
//! [`Schedulability::Unserved`] only when at least one **fresh** host
//! attestation was read and none of them covers the lane. With no readable
//! attestation the verdict is [`crate::fleet_service::Boundary`]-tagged
//! `Unknown` and the ship proceeds with a warning. A detector that cannot see
//! must say so rather than refuse, and it must equally never fold its own
//! blindness into a pass.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::fleet_service::{
    Boundary, LaneDeclaration, LaneReport, LaneServiceThresholds, RegisteredRunner, ServiceVerdict,
    assess_lane_service,
};

pub mod assess;
pub mod attestation;
pub mod gate;
pub mod gather;
pub mod workflow;

pub use assess::{AssessInput, assess};
pub use attestation::{AttestationSet, HostAttestation, LaneCoverage};
pub use gate::{GateOptions, GateOutcome};
pub use gather::{FleetFacts, gather};
pub use workflow::{RunsOnResolution, WorkflowJob, parse_workflow_jobs, resolve_runs_on_expr};

/// Exit code used when a required context cannot be scheduled onto any runner.
///
/// Distinct from [`crate::preflight::EXIT_BACKEND_UNREACHABLE`] (3) and the
/// host-health and fleet-epoch codes, because the remedy is different: nothing
/// about this host is wrong, and waiting will not fix it.
pub const EXIT_LANE_UNSERVED: u8 = 7;

/// How stale a host attestation may be before it stops counting as evidence.
///
/// Three times the 300 s tartci watchdog interval. A reader that accepted an
/// arbitrarily old file would let a host that died last week vouch for a lane
/// today.
pub const ATTESTATION_FRESH_SECS: i64 = 900;

/// Final schedulability verdict for one lane of one required context.
///
/// A superset of [`ServiceVerdict`] in meaning but deliberately its own type:
/// `Unserved` here is reached by a different route (no attestation coverage
/// rather than aged demand), and conflating the two would make the preflight's
/// refusal indistinguishable from the fleet tick's.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Schedulability {
    /// A runner that advertises every label is online in some scope.
    Served,
    /// Nothing is registered, but a fresh host attestation declares and
    /// supervises this lane: a just-in-time pool between jobs.
    Idle,
    /// Online runners advertise the labels and demand is queueing anyway. A
    /// scheduling problem — runner-group access, ephemeral consumption, a
    /// workflow permission — never a runner restore.
    Starved,
    /// No runner in either scope, and no fresh attestation covers the lane.
    /// Unschedulable now, and unschedulable in an hour.
    Unserved,
    /// The instrument could not measure. Never a pass, never on its own a
    /// refusal.
    Unknown,
}

impl Schedulability {
    /// Snake-case form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Served => "served",
            Self::Idle => "idle",
            Self::Starved => "starved",
            Self::Unserved => "unserved",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this verdict blocks submission.
    ///
    /// Only `Unserved`. `Unknown` warns loudly (it is an instrument failure,
    /// not a fleet failure) and `Starved` warns loudly (the work will be
    /// scheduled; something else is in the way).
    #[must_use]
    pub fn blocks(self) -> bool {
        matches!(self, Self::Unserved)
    }
}

/// One lane that must be schedulable for a required context to appear.
#[derive(Clone, Debug, Serialize)]
pub struct LaneAssessment {
    /// Required context this lane gates.
    pub context: String,
    /// Workflow job id whose `runs-on` this is.
    pub job_id: String,
    /// Whether the job produces the context itself, or is in its `needs`
    /// closure. Both must be schedulable; only the first is obvious.
    pub role: LaneRole,
    /// Where the label set came from: a routing variable name, `literal`, or
    /// the dynamic candidate that resolved it.
    pub source: String,
    /// Underlying service report from [`crate::fleet_service`].
    pub report: LaneReport,
    /// Final verdict after folding in host attestation.
    pub verdict: Schedulability,
    /// Hosts whose fresh attestation covers this lane, if any.
    pub attested_by: Vec<String>,
    /// Per-host attestation faults that explain an `Unserved` verdict.
    pub attestation_faults: Vec<String>,
}

/// Why a lane is in a context's closure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneRole {
    /// This job renders to the required context's name.
    Producer,
    /// This job is in the producer's transitive `needs` closure.
    Prerequisite,
}

impl LaneRole {
    /// Snake-case form used in output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Producer => "producer",
            Self::Prerequisite => "prerequisite",
        }
    }
}

/// Everything the landability preflight decided, for rendering and for JSON.
#[derive(Clone, Debug, Serialize)]
pub struct LandabilityReport {
    /// Required contexts considered, and where that list came from.
    pub contexts: Vec<String>,
    /// Which source supplied `contexts` — `branch_protection` or `config`.
    pub contexts_source: String,
    /// Every lane in every required context's closure.
    pub lanes: Vec<LaneAssessment>,
    /// Contexts for which no producing job was found in the workflows read.
    pub contexts_without_producer: Vec<String>,
    /// Hosts whose attestation was read and fresh.
    pub fresh_attesters: Vec<String>,
    /// Instrument problems worth printing even when nothing blocks.
    pub warnings: Vec<String>,
}

impl LandabilityReport {
    /// Lanes whose verdict blocks submission.
    #[must_use]
    pub fn blocking(&self) -> Vec<&LaneAssessment> {
        self.lanes
            .iter()
            .filter(|lane| lane.verdict.blocks())
            .collect()
    }

    /// Worst verdict observed, for a one-line summary.
    #[must_use]
    pub fn worst(&self) -> Schedulability {
        self.lanes
            .iter()
            .map(|lane| lane.verdict)
            .max()
            .unwrap_or(Schedulability::Unknown)
    }

    /// Render the operator-facing diagnosis for the blocking lanes.
    ///
    /// This text is the product. It names the context, every job in its
    /// closure that cannot be scheduled, the variable that routed it, the
    /// census in both scopes, the per-host attestation, and the two remedies —
    /// and states that it pulled neither.
    #[must_use]
    pub fn render_refusal(&self) -> String {
        let mut out = String::new();
        let blocking = self.blocking();
        let mut contexts: Vec<&str> = blocking
            .iter()
            .map(|lane| lane.context.as_str())
            .collect::<Vec<_>>();
        contexts.sort_unstable();
        contexts.dedup();
        for context in contexts {
            let _ = writeln!(
                out,
                "landability: required context `{context}` cannot be scheduled"
            );
            for lane in blocking.iter().filter(|lane| lane.context == context) {
                let _ = writeln!(
                    out,
                    "  job `{}` ({})  runs-on {}   <- {}",
                    lane.job_id,
                    lane.role.as_str(),
                    render_labels(&lane.report.declaration),
                    lane.source
                );
                let _ = writeln!(out, "    census: {}", lane.report.detail);
                if lane.attestation_faults.is_empty() {
                    out.push_str(
                        "    attestation: no host on this fleet declares this label set\n",
                    );
                } else {
                    for (index, fault) in lane.attestation_faults.iter().enumerate() {
                        let label = if index == 0 {
                            "attestation"
                        } else {
                            "           "
                        };
                        let _ = writeln!(out, "    {label}: {fault}");
                    }
                }
            }
        }
        out.push_str("  remedies (pick one; this tool does neither):\n");
        out.push_str(
            "    - restore a runner carrying the missing label (see the attestation lines above)\n",
        );
        out.push_str(
            "    - unset the routing variable so the job falls back to the workflow's own literal\n",
        );
        out.push_str("  contract [default] #4: held, not re-dispatched.\n");
        out.push_str(
            "  override: --allow-unserved-lane <label> proceeds and prints this as a warning.\n",
        );
        out
    }
}

fn render_labels(declaration: &LaneDeclaration) -> String {
    let labels = declaration.labels();
    if labels.is_empty() {
        format!("({})", declaration.kind())
    } else {
        format!(
            "[{}]",
            labels
                .iter()
                .map(|label| format!("\"{label}\""))
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

/// Fold host attestation into a [`LaneReport`] to produce a preflight verdict.
///
/// This is the one piece [`assess_lane_service`] cannot do on its own: at
/// preflight there is no queued demand, so its honest answer for an empty
/// census is [`ServiceVerdict::Idle`] — "indistinguishable from a just-in-time
/// pool at rest". The attestation is what distinguishes them.
///
/// The asymmetry is deliberate. Coverage by a fresh attestation *upgrades*
/// `Idle` to a proceed; absence of coverage downgrades it to `Unserved` **only
/// when at least one attestation was fresh and readable**. With no readable
/// attestation the verdict is `Unknown`, because a dead sensor must not be
/// able to block every ship on the fleet — and equally must not be able to
/// vouch for anything.
#[must_use]
pub fn fold_attestation(
    report: &LaneReport,
    attestations: &AttestationSet,
    now: DateTime<Utc>,
) -> (Schedulability, Vec<String>, Vec<String>) {
    let mut attested_by = Vec::new();
    let mut faults = Vec::new();

    let base = match report.verdict {
        ServiceVerdict::Starved => Schedulability::Starved,
        ServiceVerdict::Unserved => Schedulability::Unserved,
        ServiceVerdict::Unknown => Schedulability::Unknown,
        // `Degraded` means serving while consuming a budget, which for the
        // purpose of "can this be scheduled" is the same answer as Served.
        ServiceVerdict::Served | ServiceVerdict::Degraded => Schedulability::Served,
        ServiceVerdict::Idle => Schedulability::Idle,
    };

    // Only an `Idle` verdict is refined; every other verdict already answered
    // the question from the census alone.
    if base != Schedulability::Idle {
        return (base, attested_by, faults);
    }

    let labels = report.declaration.labels();
    if labels.is_empty() {
        return (base, attested_by, faults);
    }

    let fresh = attestations.fresh_hosts(now);
    if fresh.is_empty() {
        return (
            Schedulability::Unknown,
            attested_by,
            vec![format!(
                "no fresh host attestation readable ({}) - {}",
                attestations.describe_staleness(now),
                Boundary::Scope.next_action()
            )],
        );
    }

    for host in &fresh {
        match attestations.coverage(host, labels) {
            LaneCoverage::Supervised { detail } => {
                attested_by.push(host.clone());
                faults.push(format!("{host}: {detail}"));
            }
            LaneCoverage::Broken { detail } => faults.push(format!("{host}: {detail}")),
            LaneCoverage::NotDeclared => faults.push(format!("{host}: not declared")),
        }
    }

    if attested_by.is_empty() {
        (Schedulability::Unserved, attested_by, faults)
    } else {
        (Schedulability::Idle, attested_by, faults)
    }
}

/// Assess one lane end to end: parse the routing value, classify it against the
/// census, then fold in the host attestation.
///
/// `census` must span **both** runner scopes. A repo-scope-only census reports
/// org-registered lanes as unserved while they are online, which here would be
/// a refusal to ship on a healthy fleet.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn assess_lane(
    context: &str,
    job_id: &str,
    role: LaneRole,
    source: &str,
    raw_value: &str,
    census: &[RegisteredRunner],
    census_boundary: Option<Boundary>,
    attestations: &AttestationSet,
    thresholds: LaneServiceThresholds,
    now: DateTime<Utc>,
) -> LaneAssessment {
    let report = assess_lane_service(
        source,
        raw_value,
        census,
        census_boundary,
        // No demand exists at preflight: the work has not been queued. This is
        // the input the attestation replaces.
        &[],
        thresholds,
        now,
    );
    let (verdict, attested_by, attestation_faults) = fold_attestation(&report, attestations, now);
    LaneAssessment {
        context: context.to_owned(),
        job_id: job_id.to_owned(),
        role,
        source: source.to_owned(),
        report,
        verdict,
        attested_by,
        attestation_faults,
    }
}

#[cfg(test)]
mod tests;
