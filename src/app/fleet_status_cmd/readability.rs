//! Separating "the call did not complete" from "the answer was no".
//!
//! A GitHub read can fail for two reasons that look identical in a log line and
//! demand opposite responses. A timeout, a 5xx, or a rate limit means the fleet
//! was never asked; retrying the same call can answer it. A 401/403 means the
//! fleet was asked and refused; retrying spends quota and changes nothing.
//! Collapsing both into one `unreadable` label is what lets a 2.7-second blip
//! read as a dead fleet, so the two never share a label here.

use serde::Serialize;

/// Why an observation could not be read.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReadBoundary {
    /// The call did not complete: timeout, transport fault, 5xx, or rate limit.
    /// Nothing was asserted about the fleet, and the same call can still answer.
    Transient,
    /// GitHub answered and the answer was no: 401/403, bad credentials, or a
    /// resource this principal cannot see. Re-asking cannot change it.
    Denied,
    /// The failure text names no cause. Neither a retry nor a denial is
    /// asserted, because neither was observed.
    Unclassified,
}

impl ReadBoundary {
    /// Snake-case string form used in JSON and human output.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Denied => "denied",
            Self::Unclassified => "unclassified",
        }
    }

    /// Whether re-issuing the same call can change the answer.
    ///
    /// Only a transient failure qualifies. Retrying a denial burns the same
    /// quota the denial is sometimes caused by, and an unclassified failure
    /// gives no grounds to expect a different result.
    pub(super) fn worth_retrying(self) -> bool {
        matches!(self, Self::Transient)
    }

    /// Whether this boundary is a fleet fact rather than an observation gap.
    ///
    /// A denial persists until an operator changes a credential, so it keeps
    /// its host's health verdict. The other two say only that nobody looked.
    pub(super) fn is_fleet_fact(self) -> bool {
        matches!(self, Self::Denied)
    }
}

/// Failure texts that mean the call did not complete.
const TRANSIENT_MARKERS: [&str; 16] = [
    "rate limit",
    "rate_limit",
    "secondary rate",
    "timed out",
    "timeout",
    "deadline",
    "connection reset",
    "connection refused",
    "connection closed",
    "could not resolve host",
    "temporarily unavailable",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "server error",
    "unexpected eof",
];

/// Failure texts that mean GitHub answered no.
const DENIED_MARKERS: [&str; 8] = [
    "bad credentials",
    "resource not accessible",
    "http 401",
    "http 403",
    "forbidden",
    "unauthorized",
    "requires authentication",
    "must have admin rights",
];

/// HTTP statuses that are the server failing, not the caller being refused.
const TRANSIENT_STATUS_MARKERS: [&str; 5] =
    ["http 500", "http 502", "http 503", "http 504", "http 429"];

/// Read a failure text as one of the three boundaries.
///
/// Transient markers are tested first on purpose: GitHub serves a rate limit as
/// `HTTP 403: API rate limit exceeded`, so a denial-first order would classify
/// the single most common transient failure as a permanent one — and then stop
/// retrying exactly the case a retry fixes.
pub(super) fn classify_read_boundary(text: &str) -> ReadBoundary {
    let text = text.to_ascii_lowercase();
    if TRANSIENT_MARKERS
        .iter()
        .chain(TRANSIENT_STATUS_MARKERS.iter())
        .any(|marker| text.contains(marker))
    {
        return ReadBoundary::Transient;
    }
    if DENIED_MARKERS.iter().any(|marker| text.contains(marker)) {
        return ReadBoundary::Denied;
    }
    ReadBoundary::Unclassified
}

/// What the controller's own repo-scope census could say about the lane.
///
/// A host reports its own GitHub reads, and those reads share an
/// unauthenticated IP rate limit and the host's network. When the controller
/// reached the repo scope and found online runners for the lane, the host's
/// failed read is a second opinion that was not obtained — not evidence that
/// the lane is unserved.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LaneCorroboration {
    /// Whether the controller's repo-scope runner census answered at all.
    pub(super) inventory_readable: bool,
    /// Online runners in that census advertising the lane's labels.
    pub(super) online_lane_runners: usize,
}

impl LaneCorroboration {
    /// The repo scope answered **and** named at least one online lane runner.
    ///
    /// Both halves are required. A readable census listing no lane runner
    /// corroborates nothing, so a host's unreadable scope stays a problem and
    /// the verdict keeps failing closed.
    pub(super) fn corroborates(self) -> bool {
        self.inventory_readable && self.online_lane_runners > 0
    }

    /// Operator-facing phrasing of what the corroboration rests on.
    pub(super) fn detail(self) -> String {
        format!(
            "repo-scope runner census readable with {} online lane runner(s)",
            self.online_lane_runners
        )
    }
}

/// A host-reported GitHub read that the controller's census already answered.
///
/// It is carried rather than dropped: "I could not read X" and "X says no" must
/// keep looking different, and a gap that disappears from the output is a
/// silent upgrade to "fine".
#[derive(Clone, Debug, Serialize)]
pub(super) struct DegradedObservation {
    /// The host problem string, verbatim.
    pub(super) problem: String,
    /// How the failure text read.
    pub(super) boundary: ReadBoundary,
    /// What answered the same question in its place.
    pub(super) corroborated_by: String,
}

/// Confidence attached to a routability verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RoutingConfidence {
    /// Every input the verdict depends on was read.
    Confirmed,
    /// The verdict stands on corroboration because something could not be read.
    Degraded,
}

impl RoutingConfidence {
    /// Snake-case string form used in JSON and human output.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Degraded => "degraded",
        }
    }
}
