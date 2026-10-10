//! The coverage invariant: every open pull request Shipyard was handed is, at
//! every moment, in exactly one accounted state.
//!
//! - **progressing**: queued, required checks running, a push, a fresh red,
//!   or a fresh green inside the window the specific flags wait out;
//! - **flagged**: an owner-actionable flag holds, so the hand-back is calling
//!   its owner;
//! - **held**: a draft, or a [`HOLD_LABELS`] label;
//! - terminal (merged or closed) pull requests are out of scope.
//!
//! Anything else is a **gap**: a stuck state no specific rule names. Flag 8
//! ([`FlagKind::Unaccounted`]) catches it, so its owner is still called back,
//! and its evidence says why it fell through (merge conflicts, a red check on
//! an unarmed pull request, a cancelled or never-run required check, an armed
//! green head GitHub never queued, a removal from the queue). A gap is a rule
//! that is missing; the digest and `shipyard doctor` name each one.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::flags::{
    Arming, DigestRoute, Flag, FlagKind, HOLD_LABELS, Thresholds, all_required_green_since,
    arming_at, completed_by, latest_attempt,
};
use super::{PrHistory, QueueEventKind, RepoHistory, head_at, open_at, short};

/// Where an open handed pull request stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    /// Something is moving it.
    Progressing,
    /// A specific owner-actionable flag holds.
    Flagged,
    /// Deliberately held.
    Held,
    /// Nothing accounts for it; only flag 8 calls its owner.
    Gap,
}

/// One open handed pull request's state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageRow {
    /// Pull request.
    pub pr: u64,
    /// State.
    pub state: CoverageState,
    /// Why: what is moving it, which flag holds, the hold, or the gap.
    pub reason: String,
}

/// Whether Shipyard was handed `pr`: its author is one of
/// [`Thresholds::handed_authors`] (compared without a `[bot]` suffix).
#[must_use]
pub fn handed(pr: &PrHistory, thresholds: &Thresholds) -> bool {
    let bare = |login: &str| login.trim_end_matches("[bot]").to_ascii_lowercase();
    pr.author.as_deref().is_some_and(|author| {
        thresholds
            .handed_authors
            .iter()
            .any(|handed| bare(handed) == bare(author))
    })
}

/// The state of one open handed pull request at `at`, given the other flags
/// that hold for it (`flags`, flag 8 excluded).
#[must_use]
#[allow(clippy::too_many_lines)] // One ordered decision list.
pub fn classify(
    pr: &PrHistory,
    history: &RepoHistory,
    at: DateTime<Utc>,
    thresholds: &Thresholds,
    flags: &[Flag],
) -> CoverageRow {
    let row = |state, reason: String| CoverageRow {
        pr: pr.number,
        state,
        reason,
    };
    if pr.draft {
        return row(CoverageState::Held, "draft".to_owned());
    }
    if let Some(label) = pr.labels.iter().find(|label| {
        HOLD_LABELS
            .iter()
            .any(|hold| label.eq_ignore_ascii_case(hold))
    }) {
        return row(CoverageState::Held, format!("label `{label}`"));
    }
    let owner_flags: Vec<&str> = flags
        .iter()
        .filter(|flag| {
            flag.pr == pr.number
                && flag.kind != FlagKind::Unaccounted
                && flag.kind.owner_actionable()
                && flag.route == DigestRoute::PerPr
        })
        .map(|flag| flag.kind.as_str())
        .collect();
    if !owner_flags.is_empty() {
        return row(CoverageState::Flagged, owner_flags.join(", "));
    }
    let arming = arming_at(pr, at);
    if arming == Arming::Queued {
        return row(CoverageState::Progressing, "queued".to_owned());
    }
    let window = Duration::minutes(thresholds.green_unarmed_minutes);
    let Some(head) = head_at(pr, at) else {
        return row(
            CoverageState::Gap,
            "no head observed in the window".to_owned(),
        );
    };
    let pushed = at - head.first_seen_at;
    if pushed <= window {
        return row(
            CoverageState::Progressing,
            format!("pushed {} min ago", pushed.num_minutes()),
        );
    }
    let mut red = Vec::new();
    let mut cancelled = Vec::new();
    let mut missing = Vec::new();
    let mut latest_completion = head.first_seen_at;
    for name in &history.required_checks {
        let Some(check) = latest_attempt(head, name, at) else {
            missing.push(name.as_str());
            continue;
        };
        let Some(completed) = completed_by(check, at) else {
            return row(
                CoverageState::Progressing,
                format!("required `{name}` running"),
            );
        };
        latest_completion = latest_completion.max(completed);
        if check.failed() {
            red.push(name.as_str());
        } else if !(check.succeeded()
            || matches!(check.conclusion.as_deref(), Some("skipped" | "neutral")))
        {
            cancelled.push(name.as_str());
        }
    }
    if at - latest_completion <= window && missing.is_empty() {
        return row(
            CoverageState::Progressing,
            format!(
                "required checks settled {} min ago",
                (at - latest_completion).num_minutes()
            ),
        );
    }
    let armed = arming == Arming::Armed;
    let mut reasons = Vec::new();
    if pr.merge_state.as_deref() == Some("DIRTY") {
        reasons.push("merge conflicts with the base (DIRTY)".to_owned());
    }
    if !red.is_empty() {
        reasons.push(if armed {
            format!("required {} red", ticked(&red))
        } else {
            format!(
                "required {} red and auto-merge not armed (flag 2 covers armed pull requests only)",
                ticked(&red)
            )
        });
    }
    if !cancelled.is_empty() {
        reasons.push(format!(
            "required {} cancelled on head {} and never re-run",
            ticked(&cancelled),
            short(&head.sha)
        ));
    }
    if !missing.is_empty() {
        reasons.push(format!(
            "required {} never ran on head {}",
            ticked(&missing),
            short(&head.sha)
        ));
    }
    if reasons.is_empty() {
        let green_since = all_required_green_since(history, head, at);
        match (arming, green_since) {
            (Arming::Armed, Some(since)) => reasons.push(format!(
                "armed and green since {}, but GitHub never queued it",
                since.format("%Y-%m-%d %H:%MZ")
            )),
            (Arming::Idle, Some(_)) => {
                reasons.push("green and unarmed, but flag 6 did not hold".to_owned());
            }
            (Arming::EjectedFailedChecks, Some(_)) => {
                reasons.push("ejected and green, but flag 7 did not hold".to_owned());
            }
            _ => reasons.push("not progressing, and no rule explains why".to_owned()),
        }
    }
    if let Some(removed) = last_removal(pr, at)
        && arming == Arming::Idle
    {
        reasons.push(format!(
            "removed from the queue ({removed}) and not re-armed"
        ));
    }
    if pr.merge_state.as_deref() == Some("BEHIND") {
        reasons.push("behind the base".to_owned());
    }
    let other: Vec<&str> = flags
        .iter()
        .filter(|flag| flag.pr == pr.number && flag.kind != FlagKind::Unaccounted)
        .map(|flag| flag.kind.as_str())
        .collect();
    if !other.is_empty() {
        reasons.push(format!(
            "only flags that do not call the owner hold: {}",
            other.join(", ")
        ));
    }
    row(CoverageState::Gap, reasons.join("; "))
}

fn ticked(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The reason of the last removal from the queue that was not a merge.
fn last_removal(pr: &PrHistory, at: DateTime<Utc>) -> Option<String> {
    pr.events
        .iter()
        .filter(|event| event.at <= at)
        .filter_map(|event| match &event.kind {
            QueueEventKind::Removed { reason } if !reason.eq_ignore_ascii_case("merged") => {
                Some(reason.clone())
            }
            _ => None,
        })
        .next_back()
}

/// Flag 8 for `pr` when it is open, handed, and a gap given the flags found
/// for it so far.
#[must_use]
pub fn unaccounted_flag(
    pr: &PrHistory,
    history: &RepoHistory,
    at: DateTime<Utc>,
    thresholds: &Thresholds,
    found: &[Flag],
) -> Option<Flag> {
    if !open_at(pr, at) || !handed(pr, thresholds) {
        return None;
    }
    let row = classify(pr, history, at, thresholds, found);
    if row.state != CoverageState::Gap {
        return None;
    }
    let head = head_at(pr, at).map_or_else(|| pr.head_sha.clone(), |head| head.sha.clone());
    Some(Flag {
        pr: pr.number,
        kind: FlagKind::Unaccounted,
        key: short(&head).to_owned(),
        verdict: "stuck, and no rule explains why".to_owned(),
        evidence: row.reason,
        head_sha: head,
        route: DigestRoute::PerPr,
        shared_tests: Vec::new(),
        related_prs: Vec::new(),
    })
}

/// Every open handed pull request's state at `at`, by pull request.
#[must_use]
pub fn coverage(
    history: &RepoHistory,
    flags: &[Flag],
    at: DateTime<Utc>,
    thresholds: &Thresholds,
) -> Vec<CoverageRow> {
    history
        .prs
        .values()
        .filter(|pr| open_at(pr, at) && handed(pr, thresholds))
        .map(|pr| {
            let others: Vec<Flag> = flags
                .iter()
                .filter(|flag| flag.pr == pr.number && flag.kind != FlagKind::Unaccounted)
                .cloned()
                .collect();
            classify(pr, history, at, thresholds, &others)
        })
        .collect()
}

/// Counts by state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageSummary {
    /// When.
    pub at: Option<DateTime<Utc>>,
    /// Progressing.
    pub progressing: usize,
    /// Flagged.
    pub flagged: usize,
    /// Held.
    pub held: usize,
    /// Gaps, each with its reason.
    pub gaps: Vec<CoverageRow>,
}

/// Summarise rows.
#[must_use]
pub fn summarize(rows: &[CoverageRow], at: DateTime<Utc>) -> CoverageSummary {
    let count = |state| rows.iter().filter(|row| row.state == state).count();
    CoverageSummary {
        at: Some(at),
        progressing: count(CoverageState::Progressing),
        flagged: count(CoverageState::Flagged),
        held: count(CoverageState::Held),
        gaps: rows
            .iter()
            .filter(|row| row.state == CoverageState::Gap)
            .cloned()
            .collect(),
    }
}
