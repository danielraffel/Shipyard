//! The leak assertion: **a live object whose subject has already ended is itself
//! a defect**, and something has to be the thing that checks.
//!
//! Sibling to [`crate::fleet_service`] and shaped the same way: pure, no I/O, no
//! ambient clock, `now` injected. The caller reads the object and probes the
//! subject; this module classifies the pair.
//!
//! ## Why this is one assertion and not three
//!
//! Three leaks were found on a single day, and they look unrelated until you
//! state them the same way:
//!
//! * three "one job, then destroy" VM clones outlived their jobs by nineteen
//!   days, holding a host's entire lease budget *and* its whole VMID range;
//! * a workflow run sat queued for two days on labels nothing served and could
//!   not be cancelled, so it acted as an immortal demand signal that made a
//!   second host withhold free capacity;
//! * a `--follow` watcher polled GitHub every ten seconds for **three days and
//!   thirteen hours** past the merge of the pull request it was waiting on.
//!
//! The shared shape is exact: *something that was supposed to end, didn't, and
//! nothing reported it.* Each was found by a person happening to look. So the
//! assertion is not per-component — it is over any object with a lifetime and a
//! subject that governs it, and it needs a named **owner** who can end it.
//!
//! ## The rule that governs the whole design
//!
//! The watcher case already had a fix designed for it, and an adversarial review
//! found that the fix would emit **exit 0 for a pull request closed without
//! merging** — a false success, strictly worse than the three-day poll it
//! replaced. That is the trap this module is built to avoid:
//!
//! > **Absence is not success.** An object disappearing, a probe coming back
//! > empty, or a subject that can no longer be found are all *unknowns*. Only a
//! > positively observed, positively classified outcome is an outcome.
//!
//! [`SubjectState::Terminal`] therefore carries *how* the subject ended, and
//! [`LeakReport::succeeded`] is true only for an explicitly observed
//! [`Outcome::Succeeded`]. There is deliberately no path from "I cannot see it"
//! to a passing verdict.
//!
//! ## This is not a rare shape
//!
//! Measured on one host: of **188 ship-states, 46 — a quarter — carry empty
//! evidence and no dispatched runs**, which is precisely the shape that reports
//! "in flight" forever. 44 belong to a single repository and the oldest dates
//! back forty days. The three-day watcher was not a freak; any watcher started
//! against a quarter of that store could never hand back on local evidence
//! alone.
//!
//! ## Who wins a disagreement, and what happens to the loser
//!
//! Two sources can speak about the same lifetime, and they routinely disagree:
//! local evidence (did *our* validation pass) and the authority (did the
//! subject actually end). The precedence is fixed and narrow:
//!
//! > **The authority wins on whether the subject ended — in both directions.
//! > Local evidence is authoritative for quality, never for terminality.**
//!
//! Both directions matters. Local evidence that has gone quiet does not mean a
//! subject ended, and local evidence that passed does not either: the everyday
//! state of a pull request whose checks are green while it waits on review or
//! the merge queue is *complete locally and still open upstream*. Terminating on
//! that is the false-success this module exists to prevent.
//!
//! **The losing signal is never discarded.** Every comparison is recorded on
//! [`LeakReport::disagreement`], and some disagreements are themselves findings:
//! a subject that merged while local validation *failed*, or one that closed
//! unmerged while local validation *passed*, each say something true that the
//! winning signal alone does not. A rule that silently drops the loser is how a
//! false terminal becomes impossible to debug afterwards.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::fleet_service::{Boundary, ServiceVerdict};

/// Default grace, in seconds, allowed for an object to wind itself down after
/// its subject ends.
///
/// Orderly shutdown is not a leak. A watcher on a sixty-second interval has not
/// misbehaved five seconds after its pull request merged; one still polling
/// three days later has.
pub const DEFAULT_WINDDOWN_GRACE_SECS: i64 = 300;

/// Default largest remaining cost that still justifies draining local work.
///
/// Ten minutes: long enough for a validation leg to finish and leave evidence
/// worth reusing, short enough that nothing waits on a subject that has ended.
pub const DEFAULT_MAX_DRAIN_SECS: i64 = 600;

/// How a subject ended.
///
/// Kept separate from the fact of ending, because collapsing the two is exactly
/// how a closed-unmerged pull request becomes a success.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The subject completed its purpose: merged, passed, finished.
    Succeeded,
    /// The subject ended without achieving it: closed unmerged, failed.
    Failed,
    /// The subject was given up on: cancelled, superseded, discarded.
    Abandoned,
}

impl Outcome {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }
}

/// What the authority says about the subject right now.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubjectState {
    /// Still running, open, in flight.
    Live,
    /// Ended, with a positively observed outcome.
    Terminal {
        /// How it ended.
        outcome: Outcome,
        /// Whether this end is irreversible. A merged pull request stays
        /// merged; a closed one can be reopened, and treating that as permanent
        /// would strand a genuinely live subject as forever-terminal.
        monotonic: bool,
        /// When it ended, so a wind-down grace can be measured.
        ended_at: DateTime<Utc>,
    },
    /// The authority could not be read. Never a terminal state, and never a
    /// pass.
    Unreadable {
        /// Why the read did not answer.
        boundary: Boundary,
    },
}

/// A reference to the subject, and the authority that can speak for it.
///
/// `qualified_id` is `Option` on purpose. An under-qualified reference does not
/// fail loudly — it resolves against whatever default is ambient and answers
/// confidently about the **wrong subject**. That is a real finding from the
/// review of the watcher fix: a probe missing its repository scope can report a
/// pull request MERGED by resolving the number against the current directory's
/// repository. A wrong terminal is worse than no terminal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SubjectRef {
    /// The system that is authoritative for this subject.
    pub authority: String,
    /// The fully-qualified identity, or `None` when the reference is ambiguous.
    pub qualified_id: Option<String>,
}

/// What local evidence says about the work, independent of the subject.
///
/// [`Self::Absent`] is called out separately from [`Self::InFlight`] because it
/// is the shape a quarter of the measured store is in: no evidence and no
/// dispatched runs. Collapsing it into "in flight" is exactly the bug — it is
/// not a claim that work is running, it is the absence of any claim at all, and
/// nothing local will ever arrive to change it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalEvidence {
    /// No evidence and no dispatched runs. Local terminality is unreachable.
    Absent,
    /// Work is recorded as running.
    InFlight,
    /// Local validation finished.
    Complete {
        /// Whether it passed.
        passed: bool,
    },
}

impl LocalEvidence {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::InFlight => "in_flight",
            Self::Complete { passed: true } => "complete_pass",
            Self::Complete { passed: false } => "complete_fail",
        }
    }
}

/// Which source decided the verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// The authoritative subject.
    Subject,
    /// Neither source could answer.
    Neither,
}

/// The recorded comparison between local evidence and the subject.
///
/// Always present when both sides said something, whether or not they agreed.
/// The loser is kept verbatim so a later reader can reconstruct why a verdict
/// was reached rather than having to trust it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Disagreement {
    /// What local evidence claimed.
    pub local: LocalEvidence,
    /// Whether the two sources actually conflicted.
    pub conflicting: bool,
    /// Which source decided.
    pub winner: Authority,
    /// The signal that did not decide, preserved rather than dropped.
    pub loser_signal: String,
    /// Whether the disagreement is itself worth raising, independent of the
    /// leak verdict.
    pub raises: bool,
}

/// An object with a lifetime, whose ending is governed by a subject.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LiveObject {
    /// Identifier for the object itself.
    pub id: String,
    /// What kind of thing it is, for the operator: a watcher, a clone, a run.
    pub kind: String,
    /// **Who or what can end it.** Named because every one of these leaks was
    /// ultimately ended by a human who had to work out who was responsible; an
    /// alert that omits this makes its reader start that search over.
    pub owner: String,
    /// The subject whose ending should end this object.
    pub subject: SubjectRef,
    /// When the object came into existence.
    pub live_since: DateTime<Utc>,
    /// The concrete command that ends it, if one exists.
    pub remedy: Option<String>,
}

/// What is known about the remaining cost of local work still running.
///
/// `Unknown` is not `Unbounded`. Unbounded is a fact that justifies cancelling;
/// unknown is an absence that justifies asking, and treating the second as the
/// first would cancel work nobody measured.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "budget", rename_all = "snake_case")]
pub enum RemainingBudget {
    /// The work will finish within this many seconds.
    Bounded {
        /// Seconds remaining.
        secs_remaining: i64,
    },
    /// The work has no bound. Not drainable.
    Unbounded,
    /// Nobody measured. Not a licence to cancel.
    Unknown,
}

/// Local work that is still running after its subject ended.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LocalWork {
    /// What is running, for the operator.
    pub description: String,
    /// What it will still cost.
    pub budget: RemainingBudget,
    /// Whether the output is still wanted now that the subject has ended.
    pub output_still_wanted: bool,
}

/// What to do with local work whose subject has already ended.
///
/// The third quadrant is the only one that needs a policy rather than a rule,
/// and the one thing forbidden in all cases is silently abandoning the work —
/// a run that vanished with neither outcome is the leak in miniature.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "reclamation", rename_all = "snake_case")]
pub enum Reclamation {
    /// Let it finish, then hand back. Its evidence is still worth having.
    Drain {
        /// Why draining is the right call.
        reason: String,
    },
    /// Stop it. Its output is worthless or its cost is unbounded.
    Cancel {
        /// Why cancelling is the right call.
        reason: String,
    },
    /// Neither can be justified from what is known. Ask rather than act.
    Escalate {
        /// What is missing.
        reason: String,
    },
}

impl Reclamation {
    /// Snake-case discriminant for JSON and metrics.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Drain { .. } => "drain",
            Self::Cancel { .. } => "cancel",
            Self::Escalate { .. } => "escalate",
        }
    }
}

/// Decide what to do with local work left running by an ended subject.
///
/// Default: **drain if the remaining cost is provably bounded and small,
/// cancel otherwise.** Unbounded work is not drainable by definition, and work
/// whose cost nobody measured is escalated rather than cancelled — the same
/// rule the bounded self-heal gate applies to idleness, for the same reason.
#[must_use]
pub fn decide_reclamation(work: &LocalWork, thresholds: LeakThresholds) -> Reclamation {
    if !work.output_still_wanted {
        return Reclamation::Cancel {
            reason: format!(
                "`{}` is still running but its output is no longer wanted now the subject has \
                 ended",
                work.description
            ),
        };
    }
    match work.budget {
        RemainingBudget::Bounded { secs_remaining }
            if secs_remaining <= thresholds.max_drain_secs =>
        {
            Reclamation::Drain {
                reason: format!(
                    "`{}` finishes in {secs_remaining}s, within the {}s drain budget, and its \
                     evidence is still wanted",
                    work.description, thresholds.max_drain_secs
                ),
            }
        }
        RemainingBudget::Bounded { secs_remaining } => Reclamation::Cancel {
            reason: format!(
                "`{}` needs {secs_remaining}s, beyond the {}s drain budget; the subject has \
                 already ended, so paying it buys nothing",
                work.description, thresholds.max_drain_secs
            ),
        },
        RemainingBudget::Unbounded => Reclamation::Cancel {
            reason: format!(
                "`{}` has no bound, and unbounded work cannot be drained — that is how a leak \
                 becomes permanent",
                work.description
            ),
        },
        RemainingBudget::Unknown => Reclamation::Escalate {
            reason: format!(
                "`{}` has no measured remaining cost, and an absence of measurement is not a \
                 licence to cancel work somebody may still need",
                work.description
            ),
        },
    }
}

/// Tunables for [`assess_live_object`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeakThresholds {
    /// How long an object may remain live after its subject ended.
    pub winddown_grace_secs: i64,
    /// The largest remaining cost that still justifies draining local work
    /// rather than cancelling it.
    pub max_drain_secs: i64,
}

impl Default for LeakThresholds {
    fn default() -> Self {
        Self {
            winddown_grace_secs: DEFAULT_WINDDOWN_GRACE_SECS,
            max_drain_secs: DEFAULT_MAX_DRAIN_SECS,
        }
    }
}

/// What the object is doing relative to its subject.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeakState {
    /// Subject live, object live. Working as intended.
    Tracking,
    /// Subject ended, object still live but inside its wind-down grace.
    WindingDown,
    /// Subject ended, object still live past its grace. The leak.
    Leaked,
    /// The subject's state could not be established, so no claim is made.
    Undetermined,
}

impl LeakState {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tracking => "tracking",
            Self::WindingDown => "winding_down",
            Self::Leaked => "leaked",
            Self::Undetermined => "undetermined",
        }
    }
}

/// Verdict for one live object.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LeakReport {
    /// The object's identifier.
    pub object_id: String,
    /// What kind of object it is.
    pub kind: String,
    /// Who can end it.
    pub owner: String,
    /// What it is doing relative to its subject.
    pub state: LeakState,
    /// The shared verdict.
    pub verdict: ServiceVerdict,
    /// Why no claim could be made, when the verdict is `Unknown`.
    pub boundary: Option<Boundary>,
    /// How the subject ended, when that was positively observed.
    ///
    /// `None` whenever the outcome was not established — including every case
    /// where the object merely vanished or the probe came back empty.
    pub outcome: Option<Outcome>,
    /// How long the object has outlived its subject, in seconds.
    pub leaked_for_secs: Option<i64>,
    /// The recorded local-versus-subject comparison. Never dropped.
    pub disagreement: Option<Disagreement>,
    /// Operator-facing explanation.
    pub detail: String,
    /// What to do about it.
    pub next_action: String,
}

impl LeakReport {
    /// Whether this report may be treated as a success.
    ///
    /// True only for a positively observed [`Outcome::Succeeded`]. Every other
    /// case — failed, abandoned, unreadable, ambiguous, or simply absent — is
    /// not a success, and the absence of a report is not one either. This is the
    /// single guard against the false-success terminal that the review of the
    /// narrow fix found: emitting exit 0 for a pull request that was closed
    /// **without** merging is strictly worse than never exiting at all.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.outcome == Some(Outcome::Succeeded)
    }
}

/// Classify one live object against its subject.
#[must_use]
pub fn assess_live_object(
    object: &LiveObject,
    local: LocalEvidence,
    subject: &SubjectState,
    thresholds: LeakThresholds,
    now: DateTime<Utc>,
) -> LeakReport {
    let mut report = LeakReport {
        object_id: object.id.clone(),
        kind: object.kind.clone(),
        owner: object.owner.clone(),
        state: LeakState::Undetermined,
        verdict: ServiceVerdict::Unknown,
        boundary: None,
        outcome: None,
        leaked_for_secs: None,
        disagreement: None,
        detail: String::new(),
        next_action: String::new(),
    };

    // An ambiguous reference resolves against whatever default is ambient and
    // answers about the wrong subject. Refuse before reading it.
    let Some(qualified) = object.subject.qualified_id.as_deref() else {
        report.boundary = Some(Boundary::Scope);
        report.detail = format!(
            "{} `{}` names no fully-qualified subject on authority `{}`, so any answer would \
             resolve against an ambient default and could report an unrelated subject as ended",
            object.kind, object.id, object.subject.authority
        );
        report.next_action =
            "qualify the subject reference before probing; a wrong terminal is worse than none"
                .to_owned();
        return report;
    };

    match subject {
        SubjectState::Unreadable { boundary } => {
            report.disagreement = Some(Disagreement {
                local,
                conflicting: false,
                winner: Authority::Neither,
                loser_signal: format!(
                    "local evidence reads `{}`, but it cannot settle terminality and the \
                     authority did not answer, so neither source decided",
                    local.as_str()
                ),
                raises: false,
            });
            report.boundary = Some(*boundary);
            report.detail = format!(
                "{} `{}` is live and its subject `{qualified}` could not be read ({}), so no \
                 claim is made either way",
                object.kind,
                object.id,
                boundary.as_str()
            );
            report.next_action = boundary.next_action().to_owned();
        }
        SubjectState::Live => {
            report.disagreement = Some(compare(local, None));
            report.state = LeakState::Tracking;
            report.verdict = ServiceVerdict::Served;
            report.detail = format!(
                "{} `{}` is tracking subject `{qualified}`, which is still live",
                object.kind, object.id
            );
            report.next_action = "nothing".to_owned();
        }
        SubjectState::Terminal {
            outcome, ended_at, ..
        } => {
            let outlived = (now - *ended_at).num_seconds();
            report.disagreement = Some(compare(local, Some(*outcome)));
            report.outcome = Some(*outcome);
            report.leaked_for_secs = Some(outlived);
            if outlived < thresholds.winddown_grace_secs {
                report.state = LeakState::WindingDown;
                report.verdict = ServiceVerdict::Served;
                report.detail = format!(
                    "{} `{}` is still live {outlived}s after subject `{qualified}` {}, inside \
                     the {}s wind-down grace",
                    object.kind,
                    object.id,
                    outcome.as_str(),
                    thresholds.winddown_grace_secs
                );
                report.next_action = "nothing yet; orderly shutdown is not a leak".to_owned();
            } else {
                report.state = LeakState::Leaked;
                report.verdict = ServiceVerdict::Degraded;
                report.detail = format!(
                    "{} `{}` has outlived subject `{qualified}` by {outlived}s; the subject {} \
                     and nothing ended the object. Owner: {}",
                    object.kind,
                    object.id,
                    outcome.as_str(),
                    object.owner
                );
                report.next_action = object.remedy.clone().unwrap_or_else(|| {
                    format!(
                        "no recorded remedy — {} must be given one, or this leaks again",
                        object.owner
                    )
                });
            }
        }
    }

    report
}

/// Reconcile a previously observed terminal against a fresh observation.
///
/// Subjects reopen. A pull request closed on Monday and reopened on Tuesday is
/// live, and a terminal mark that cannot be revoked would strand it as
/// permanently ended — which then leaks in the opposite direction, ending an
/// object whose subject is running.
///
/// A **monotonic** terminal is never revoked, so a fresh reading that fails to
/// see a merged subject cannot resurrect it. A non-monotonic one yields to the
/// newer observation.
#[must_use]
pub fn reconcile_subject(previous: Option<&SubjectState>, fresh: SubjectState) -> SubjectState {
    let Some(SubjectState::Terminal {
        outcome,
        monotonic: true,
        ended_at,
    }) = previous
    else {
        return fresh;
    };
    // Monotonic terminals stand, whatever a later read claims.
    SubjectState::Terminal {
        outcome: *outcome,
        monotonic: true,
        ended_at: *ended_at,
    }
}

/// Whether a successor object may inherit a predecessor's subject state.
///
/// It may not, ever. A replacement created from a terminated predecessor would
/// otherwise be **born terminal** and be reaped before it did any work. The
/// review of the narrow fix caught exactly this: the archive-and-replace path
/// would have copied the terminal mark forward, and that code already nulls a
/// neighbouring field for the same reason.
#[must_use]
pub fn subject_state_for_successor() -> Option<SubjectState> {
    None
}

/// Compare local evidence against the authority, and record the loser.
///
/// The authority always wins on terminality. What varies is whether the losing
/// signal is merely context or a finding in its own right:
///
/// | local | subject | raises? | because |
/// |---|---|---|---|
/// | absent | ended | yes | local terminality was unreachable — the shape a quarter of the store is in |
/// | complete (pass) | ended failed | yes | our validation passed and it still did not land |
/// | complete (fail) | ended succeeded | yes | it landed although our validation failed |
/// | complete (either) | live | no | the everyday wait on review, required checks, or the queue |
/// | in flight | ended | no | ordinary: the subject beat our evidence home |
fn compare(local: LocalEvidence, ended: Option<Outcome>) -> Disagreement {
    let (conflicting, raises, loser_signal) = match (local, ended) {
        (LocalEvidence::Absent, Some(outcome)) => (
            true,
            true,
            format!(
                "local evidence was absent (no evidence, no dispatched runs), so nothing local \
                 could ever have marked this terminal; the subject {} and only the authority \
                 knew",
                outcome.as_str()
            ),
        ),
        (LocalEvidence::Complete { passed: true }, Some(Outcome::Failed | Outcome::Abandoned)) => (
            true,
            true,
            "local validation PASSED but the subject did not land — worth understanding before \
             the next attempt repeats it"
                .to_owned(),
        ),
        (LocalEvidence::Complete { passed: false }, Some(Outcome::Succeeded)) => (
            true,
            true,
            "the subject SUCCEEDED although local validation failed — something landed past a \
             failing gate"
                .to_owned(),
        ),
        (LocalEvidence::Complete { passed }, None) => (
            true,
            false,
            format!(
                "local validation is complete ({}) while the subject is still open; that is the \
                 ordinary wait on review, required checks, or the queue, and it is NOT terminal",
                if passed { "passed" } else { "failed" }
            ),
        ),
        (LocalEvidence::Absent, None) => (
            false,
            false,
            "local evidence is absent; terminality here can only ever come from the authority"
                .to_owned(),
        ),
        (LocalEvidence::InFlight, Some(outcome)) => (
            true,
            false,
            format!(
                "local evidence still reads in-flight while the subject {}; the authority is \
                 ahead of our record",
                outcome.as_str()
            ),
        ),
        (LocalEvidence::InFlight, None) => (
            false,
            false,
            "local evidence and the authority agree the work is still live".to_owned(),
        ),
        (LocalEvidence::Complete { .. }, Some(_)) => (
            false,
            false,
            "local evidence and the authority agree on how this ended".to_owned(),
        ),
    };
    Disagreement {
        local,
        conflicting,
        winner: Authority::Subject,
        loser_signal,
        raises,
    }
}

/// Worst verdict across a set of objects.
///
/// Empty is [`ServiceVerdict::Unknown`]: having checked nothing is not the same
/// as having found nothing wrong.
#[must_use]
pub fn roll_up(reports: &[LeakReport]) -> ServiceVerdict {
    reports
        .iter()
        .map(|report| report.verdict)
        .max()
        .unwrap_or(ServiceVerdict::Unknown)
}

#[cfg(test)]
mod tests;
