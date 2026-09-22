//! The landing model: how work actually merges in one repository.
//!
//! Shipyard already answers "is my validation green" ([`crate::ship`]) and
//! "can my required contexts be scheduled" ([`crate::landability`]). Neither
//! answers the question an agent has to settle before it does any work at all:
//!
//! > in THIS repository, what is the mechanism by which a pull request
//! > becomes a commit on the default branch?
//!
//! ## Why this needs measuring rather than reading
//!
//! Every written copy of that answer drifts. A repository's decisions
//! document, its CI guide and its agent instructions all describe the merge
//! mechanism at the moment they were written, and none of them is updated when
//! somebody flips a setting in the web UI. An agent that trusts the prose does
//! the wrong work confidently, and the wrong work here is expensive: with a
//! merge queue live and up-to-date protection on, hand-rebasing a backlog one
//! pull request at a time is a treadmill, because each merge puts every other
//! open pull request `BEHIND` and forces another full-gate revalidation. The
//! queue exists precisely to batch that.
//!
//! ## Why branch protection is the wrong place to look
//!
//! `GET /repos/{owner}/{repo}/branches/{branch}/protection` has **no field**
//! that can carry a merge queue. Its payload is `required_status_checks`,
//! `required_pull_request_reviews`, `enforce_admins`, `required_linear_history`
//! and friends — a queue configured as a repository **ruleset** appears in
//! none of them. The endpoint does not return `false` for the queue; it says
//! nothing, and silence reads as `false` to anybody who only asks there.
//!
//! So this module treats branch protection as a surface that **cannot vote**
//! on queue presence. It supplies the strict up-to-date flag and the required
//! contexts, and its silence about a queue is recorded as inexpressible rather
//! than as absence. The queue itself is read from
//! `GET /repos/{owner}/{repo}/rulesets` plus the per-ruleset detail call, and
//! corroborated against GraphQL `repository.mergeQueue(branch:)`.
//!
//! ## Fail closed
//!
//! An unreadable surface is [`Verdict::Unknown`], never [`Verdict::Absent`].
//! Reporting "no merge queue" when the truth is "could not read rulesets" is
//! the exact false negative this module exists to end, and it is worse than
//! reporting nothing: absence is actionable, so an agent acts on it.
//! Consequently a queue verdict only settles on `Absent` when every surface
//! that can express a queue was read and none of them found one.

use serde::Serialize;

use crate::fleet_service::Boundary;

pub mod backlog;
pub mod gather;
pub mod placement;
pub mod queue;
pub mod render;

#[cfg(test)]
mod tests;

/// Report generation. A consumer has to be able to tell a report produced by
/// this build from one produced by a predecessor that lacked a field, because
/// a missing block reads as "nothing to report" rather than "not measured".
pub const SCHEMA_VERSION: u32 = 1;

/// Exit code used when the landing mechanism could not be determined.
///
/// Distinct from a clean run so a caller can branch on "I do not know how this
/// repo lands work" without parsing prose.
pub const EXIT_LANDING_UNKNOWN: u8 = 9;

/// A three-valued answer that can say it does not know.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", content = "value", rename_all = "snake_case")]
pub enum Verdict<T> {
    /// The surfaces were read and the thing is there.
    Present(T),
    /// Every surface that can express the thing was read, and none found it.
    Absent,
    /// At least one surface could not be read, so absence cannot be asserted.
    Unknown {
        /// Which class of limit stopped the read.
        boundary: Boundary,
        /// What was attempted and what came back.
        detail: String,
    },
}

impl<T> Verdict<T> {
    /// Whether this verdict is [`Verdict::Unknown`].
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }

    /// The carried value when present.
    #[must_use]
    pub const fn present(&self) -> Option<&T> {
        match self {
            Self::Present(value) => Some(value),
            _ => None,
        }
    }
}

/// One GitHub surface this report consulted, and what it yielded.
///
/// Recorded per surface rather than collapsed into a single verdict so a
/// reader can see which endpoint answered. A verdict with no provenance is
/// indistinguishable from a guess.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SurfaceRead {
    /// Endpoint family, for example `rulesets` or `branch_protection`.
    pub surface: String,
    /// Exact path or GraphQL root that was queried.
    pub query: String,
    /// Whether the read succeeded, and why not when it did not.
    pub outcome: SurfaceOutcome,
}

/// The result of consulting one surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SurfaceOutcome {
    /// The surface answered and the answer was parsed.
    Read,
    /// The surface answered, and the answer structurally cannot carry the
    /// fact being sought. Deliberately not `Read`: a surface that cannot
    /// express a queue must never be counted as having found no queue.
    Inexpressible {
        /// What this surface is structurally unable to say.
        detail: String,
    },
    /// The surface did not answer.
    Unreadable {
        /// Which class of limit stopped the read.
        boundary: Boundary,
        /// What came back instead.
        detail: String,
    },
}

/// The complete landing model for one repository and base branch.
#[derive(Clone, Debug, Serialize)]
pub struct LandingReport {
    /// Report generation.
    pub schema_version: u32,
    /// `OWNER/REPO`.
    pub repo: String,
    /// Base branch the model describes.
    pub base: String,
    /// Merge-queue configuration, measured across every surface that can
    /// express one.
    pub merge_queue: queue::QueueFinding,
    /// Up-to-date (strict) protection, and what it implies.
    pub strict: queue::StrictFinding,
    /// How a pull request is put on the path to merge here.
    pub enqueue: queue::EnqueueGuidance,
    /// Where each required context was last observed executing.
    pub required_checks: placement::PlacementFinding,
    /// Open pull requests grouped by mergeability.
    pub backlog: backlog::BacklogFinding,
    /// Every surface consulted, in the order it was consulted.
    pub surfaces: Vec<SurfaceRead>,
    /// Instrument problems worth printing alongside the findings.
    pub warnings: Vec<String>,
    /// How many API calls this report actually cost. Reported rather than
    /// estimated: a budget nobody measures is a wish.
    pub api_calls: u32,
}

impl LandingReport {
    /// Whether any headline finding is unknown.
    ///
    /// The backlog is deliberately excluded: an unreadable pull-request list
    /// leaves the merge mechanism perfectly well described, and refusing over
    /// it would make the command useless on a repository whose pull requests
    /// are large enough to time out.
    #[must_use]
    pub fn has_unknown(&self) -> bool {
        self.merge_queue.verdict.is_unknown() || self.strict.verdict.is_unknown()
    }
}
