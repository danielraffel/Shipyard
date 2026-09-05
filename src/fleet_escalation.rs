//! Decide when a fleet service verdict should leave the machine and reach a
//! human who is not looking.
//!
//! Pure, like its siblings [`crate::fleet_service`] and
//! [`crate::runner_watchdog`]: this module decides *what* to escalate and
//! *when*. The caller performs the I/O — opening, editing and closing the
//! tracking issue — so the policy is testable without a network.
//!
//! ## Why escalation has to leave the host
//!
//! A journal line on a broken machine is not a signal, because nobody reads
//! that machine. Every fault this system detects was ultimately found by a
//! person deciding to look: a Linux lane dead for ~19 days was found when
//! someone asked "are all machines picking up work?", and a second host with
//! the identical supervisor fault was found hours after the first only because
//! someone thought to re-check its sibling. Repairing one instance does not
//! surface the others.
//!
//! The bar this module exists to meet: **a lane unserved for an hour should
//! cost a human one glance, not nineteen days and a lucky question.**
//!
//! ## Why hysteresis is the whole design, not a refinement
//!
//! Every signal measured on this fleet flickers. A supervisor was blind on
//! 1598 of its last 2000 log cycles while its final 70 lines read perfectly
//! healthy. A pool alternates between refusing and serving. A just-in-time
//! runner registers and deregisters around every job.
//!
//! An escalation that reacts to a single sample would therefore open and close
//! an issue repeatedly, and **a flapping alarm is worse than no alarm**: it is
//! ignored, and then the one real occurrence is ignored with it. So a subject
//! must be raising *continuously* for [`EscalationThresholds::raise_after_secs`]
//! before anything is opened, and healthy *continuously* for
//! [`EscalationThresholds::clear_after_secs`] before it is closed.
//!
//! The two are deliberately asymmetric — slower to clear than to raise — so a
//! fault that briefly looks fixed does not close its own issue and start over.
//!
//! ## Why an unchanged issue is left alone
//!
//! Re-posting the same body on every cycle buries the one edit that mattered
//! and trains the reader to skip the notification. An [`EscalationAction::Update`]
//! is emitted only when the rendered body actually changed.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::fleet_service::{Boundary, ServiceVerdict};

/// Default continuous-raising time before a tracking issue is opened.
///
/// Fifteen minutes leaves generous margin under the one-hour bar while sitting
/// well above the flicker of a just-in-time pool or an intermittent scan.
pub const DEFAULT_RAISE_AFTER_SECS: i64 = 900;

/// Default continuous-healthy time before a tracking issue is closed.
///
/// Twice the raise threshold: closing eagerly on a fault that merely looks
/// fixed produces the open/close churn this module exists to avoid.
pub const DEFAULT_CLEAR_AFTER_SECS: i64 = 1800;

/// Tunables for [`decide_escalation`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EscalationThresholds {
    /// How long a subject must raise continuously before an issue is opened.
    pub raise_after_secs: i64,
    /// How long a subject must be healthy continuously before it is closed.
    pub clear_after_secs: i64,
}

impl Default for EscalationThresholds {
    fn default() -> Self {
        Self {
            raise_after_secs: DEFAULT_RAISE_AFTER_SECS,
            clear_after_secs: DEFAULT_CLEAR_AFTER_SECS,
        }
    }
}

/// The current state of one thing being watched — a host, or a lane on a host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SubjectState {
    /// Stable identity for this subject, used to match an existing issue.
    ///
    /// Must not embed anything that changes between cycles (a timestamp, a run
    /// id, a queue depth); a key that drifts opens a new issue every cycle,
    /// which is the failure this field prevents.
    pub key: String,
    /// Host the subject lives on, named in the escalation.
    pub host: String,
    /// Lane or unit the subject describes.
    pub lane: String,
    /// Current verdict.
    pub verdict: ServiceVerdict,
    /// Boundary, when the verdict is [`ServiceVerdict::Unknown`].
    pub boundary: Option<Boundary>,
    /// Operator-facing description of what was measured.
    pub detail: String,
    /// What the system already attempted, if anything. Named in the body so a
    /// reader is not left guessing whether a self-heal was tried.
    pub attempted: Vec<String>,
    /// Concrete next action for a human.
    pub next_action: String,
    /// When this subject began raising continuously. `None` when healthy.
    pub raising_since: Option<DateTime<Utc>>,
    /// When this subject last became healthy continuously. `None` when raising.
    pub healthy_since: Option<DateTime<Utc>>,
}

/// A tracking issue already open for a subject.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TrackingIssue {
    /// Issue number.
    pub number: u64,
    /// The subject key this issue tracks.
    pub key: String,
    /// The body currently posted, so an unchanged one is left alone.
    pub body: String,
}

/// What the caller should do about a subject.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum EscalationAction {
    /// Open a new tracking issue.
    Open {
        /// Subject key, embedded in the body as a machine-readable marker.
        key: String,
        /// Issue title.
        title: String,
        /// Issue body.
        body: String,
    },
    /// Edit an existing issue in place, because its content changed.
    Update {
        /// Issue to edit.
        number: u64,
        /// Replacement body.
        body: String,
    },
    /// Close an issue: the subject recovered and stayed recovered.
    Close {
        /// Issue to close.
        number: u64,
        /// Closing comment.
        comment: String,
    },
    /// Do nothing, and say why. Every cycle produces a decision, and a
    /// decision that cannot explain itself is how a silent watchdog looks from
    /// the outside.
    Nothing {
        /// Why no action was taken.
        reason: String,
    },
}

impl EscalationAction {
    /// Snake-case discriminant for JSON output and metrics.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::Update { .. } => "update",
            Self::Close { .. } => "close",
            Self::Nothing { .. } => "nothing",
        }
    }

    /// Whether this action changes anything outside the process.
    #[must_use]
    pub fn is_mutation(&self) -> bool {
        !matches!(self, Self::Nothing { .. })
    }
}

/// Machine-readable marker embedded in every body so an issue can be matched
/// back to its subject without depending on the title, which a human may edit.
#[must_use]
pub fn subject_marker(key: &str) -> String {
    format!("<!-- shipyard-fleet-subject: {key} -->")
}

/// Decide what to do about one subject.
///
/// `existing` is the open tracking issue for this subject, if the caller found
/// one. Passing an issue whose `key` differs from the subject's is treated as
/// no issue at all — matching the wrong issue would edit an unrelated report.
#[must_use]
pub fn decide_escalation(
    subject: &SubjectState,
    existing: Option<&TrackingIssue>,
    thresholds: EscalationThresholds,
    now: DateTime<Utc>,
) -> EscalationAction {
    let existing = existing.filter(|issue| issue.key == subject.key);

    if subject.verdict.is_raise() {
        return decide_while_raising(subject, existing, thresholds, now);
    }
    decide_while_healthy(subject, existing, thresholds, now)
}

fn decide_while_raising(
    subject: &SubjectState,
    existing: Option<&TrackingIssue>,
    thresholds: EscalationThresholds,
    now: DateTime<Utc>,
) -> EscalationAction {
    let Some(since) = subject.raising_since else {
        // A raising verdict with no start time is an incomplete observation,
        // not a healthy one. Do not open on it, and do not silently drop it.
        return EscalationAction::Nothing {
            reason: format!(
                "{} is {} but no raising-since timestamp was supplied, so its duration \
                 is unknown and the raise threshold cannot be evaluated",
                subject.key,
                subject.verdict.as_str()
            ),
        };
    };

    let raising_for = (now - since).num_seconds();
    if raising_for < thresholds.raise_after_secs {
        return EscalationAction::Nothing {
            reason: format!(
                "{} has been {} for {raising_for}s, under the {}s raise threshold — \
                 every signal on this fleet flickers, so a single sample is not a fault",
                subject.key,
                subject.verdict.as_str(),
                thresholds.raise_after_secs
            ),
        };
    }

    let body = render_body(subject, raising_for);
    match existing {
        None => EscalationAction::Open {
            key: subject.key.clone(),
            title: render_title(subject),
            body,
        },
        Some(issue) if issue.body != body => EscalationAction::Update {
            number: issue.number,
            body,
        },
        Some(issue) => EscalationAction::Nothing {
            reason: format!(
                "issue #{} already reports this exact state; re-posting an unchanged body \
                 buries the edit that matters",
                issue.number
            ),
        },
    }
}

fn decide_while_healthy(
    subject: &SubjectState,
    existing: Option<&TrackingIssue>,
    thresholds: EscalationThresholds,
    now: DateTime<Utc>,
) -> EscalationAction {
    let Some(issue) = existing else {
        return EscalationAction::Nothing {
            reason: format!(
                "{} is {} and nothing is open for it",
                subject.key,
                subject.verdict.as_str()
            ),
        };
    };

    let Some(since) = subject.healthy_since else {
        return EscalationAction::Nothing {
            reason: format!(
                "{} is {} but no healthy-since timestamp was supplied, so the recovery \
                 cannot be shown to have held; issue #{} stays open",
                subject.key,
                subject.verdict.as_str(),
                issue.number
            ),
        };
    };

    let healthy_for = (now - since).num_seconds();
    if healthy_for < thresholds.clear_after_secs {
        return EscalationAction::Nothing {
            reason: format!(
                "{} has been {} for only {healthy_for}s, under the {}s clear threshold — \
                 closing on a fault that merely looks fixed restarts the cycle",
                subject.key,
                subject.verdict.as_str(),
                thresholds.clear_after_secs
            ),
        };
    }

    EscalationAction::Close {
        number: issue.number,
        comment: format!(
            "Recovered: `{}` on `{}` has read `{}` continuously for {healthy_for}s \
             (clear threshold {}s).\n\n{}\n\nClosing automatically. It will reopen if the \
             lane degrades again.",
            subject.lane,
            subject.host,
            subject.verdict.as_str(),
            thresholds.clear_after_secs,
            subject.detail,
        ),
    }
}

fn render_title(subject: &SubjectState) -> String {
    format!(
        "Fleet: {} on {} is {}",
        subject.lane,
        subject.host,
        subject.verdict.as_str()
    )
}

/// Render the issue body.
///
/// The body must name the host, the lane, what was tried, and the next action a
/// human should take. An alert that only says something is wrong makes its
/// reader start the investigation from nothing, which is most of the cost of
/// the incidents this system exists to prevent.
fn render_body(subject: &SubjectState, raising_for: i64) -> String {
    use std::fmt::Write as _;

    let mut body = String::new();
    body.push_str(&subject_marker(&subject.key));
    body.push_str("\n\n");
    // Writing into a String cannot fail, so the results are discarded rather
    // than propagated through a signature that has no other error case.
    let _ = writeln!(
        body,
        "**Host:** `{}`\n**Lane:** `{}`\n**Verdict:** `{}`\n**Continuously for:** {raising_for}s",
        subject.host,
        subject.lane,
        subject.verdict.as_str(),
    );

    if let Some(boundary) = subject.boundary {
        let _ = writeln!(
            body,
            "**Could not measure — boundary:** `{}`\n\nThis is not a pass. {}",
            boundary.as_str(),
            boundary.next_action()
        );
    }

    let _ = writeln!(body, "\n**What was measured**\n\n{}", subject.detail);

    body.push_str("\n**What was already tried**\n\n");
    if subject.attempted.is_empty() {
        body.push_str(
            "Nothing — no bounded self-heal applies to this verdict, or the target \
             could not be proven idle.\n",
        );
    } else {
        for step in &subject.attempted {
            let _ = writeln!(body, "- {step}");
        }
    }

    let _ = writeln!(body, "\n**Next action**\n\n{}", subject.next_action);
    body
}

/// Decide for a whole set of subjects.
///
/// One issue per subject key: a single roll-up issue for the fleet would hide
/// the second instance of a fault behind the first, which is exactly how a
/// supervisor fault on one host stayed hidden after its twin was repaired.
#[must_use]
pub fn decide_all(
    subjects: &[SubjectState],
    existing: &[TrackingIssue],
    thresholds: EscalationThresholds,
    now: DateTime<Utc>,
) -> Vec<EscalationAction> {
    subjects
        .iter()
        .map(|subject| {
            let issue = existing.iter().find(|issue| issue.key == subject.key);
            decide_escalation(subject, issue, thresholds, now)
        })
        .collect()
}

#[cfg(test)]
mod tests;
