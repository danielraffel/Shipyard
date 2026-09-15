//! Terminal-verdict consumption: which finished ships still need someone to look.
//!
//! Shipyard already records what every validation concluded. The gap this
//! module closes is on the reading side: a ship that ran to completion and
//! **failed** is not orphaned, so it never appears in `ship-state list`, and no
//! component outlives the agent that dispatched it to notice. The failure is
//! recorded correctly and consumed by nobody.
//!
//! Two properties are deliberate.
//!
//! A verdict alone is not actionable. Measured against a live store, 41 of 45
//! resolvable failure records belonged to pull requests that had since merged —
//! a list of raw failures would be ~91% stale, which is how an existing
//! diagnostic earns the habit of being ignored. So a verdict is paired with the
//! pull request's disposition, and only a non-passing verdict on a still-open
//! pull request is reported as actionable.
//!
//! Resolution that did not happen is never silently folded into "clean".
//! [`VerdictCensus`] counts unresolved records separately, so a resolver that
//! is broken, throttled, or switched off reads as *unresolved*, never as an
//! empty actionable list. That distinction is the whole point: a consumer whose
//! own blindness is invisible reproduces the bug it was built to fix.

use crate::ship_state::ShipState;
use crate::watch::ship_terminal_verdict;

/// Run status string that records a lane cancelled rather than concluded.
const CANCELLED_RUN_STATUS: &str = "cancelled";

/// What a finished validation concluded.
///
/// `Cancelled` is kept distinct from `Failed` on purpose. Both are terminal and
/// neither merges, but they ask for different next moves — a cancellation is
/// re-run, a failure is investigated — and collapsing them hides which one
/// happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalVerdict {
    /// Every required lane passed.
    Passed,
    /// At least one required lane concluded without passing.
    Failed,
    /// At least one required lane was cancelled rather than concluded.
    Cancelled,
}

impl TerminalVerdict {
    /// Stable lowercase label used in rendered and machine-readable output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether this verdict still needs a human or agent to act.
    ///
    /// A pass needs nobody: a green gate announces itself by the pull request
    /// merging.
    #[must_use]
    pub fn needs_attention(self) -> bool {
        !matches!(self, Self::Passed)
    }
}

/// Where a pull request stands, independent of what its validation concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrDisposition {
    /// Still open, so a non-passing verdict still blocks it.
    Open,
    /// Already merged, so an older failing verdict is spent history.
    Merged,
    /// Closed without merging.
    Closed,
    /// Could not be resolved. Never treated as "no longer relevant".
    Unknown,
}

impl PrDisposition {
    /// Parse GitHub's `state` field. Unrecognised input stays [`Self::Unknown`].
    #[must_use]
    pub fn from_state(state: &str) -> Self {
        if state.eq_ignore_ascii_case("open") {
            Self::Open
        } else if state.eq_ignore_ascii_case("merged") {
            Self::Merged
        } else if state.eq_ignore_ascii_case("closed") {
            Self::Closed
        } else {
            Self::Unknown
        }
    }

    /// Stable lowercase label used in rendered and machine-readable output.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Unknown => "unknown",
        }
    }
}

/// One finished ship, paired with where its pull request now stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictRow {
    /// Repository slug the ship belonged to.
    pub repo: String,
    /// Pull request number.
    pub pr: u64,
    /// What the validation concluded.
    pub verdict: TerminalVerdict,
    /// Where the pull request stands now.
    pub disposition: PrDisposition,
    /// Pull request title, when the record carried one.
    pub title: String,
    /// Pull request URL, when the record carried one.
    pub url: String,
}

impl VerdictRow {
    /// Whether this row still needs someone to act.
    ///
    /// Actionable requires both halves: a verdict that did not pass, and a pull
    /// request still open. A failure on a merged pull request is spent, and an
    /// unresolved disposition is reported as unresolved rather than silently
    /// promoted or dropped.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        self.verdict.needs_attention() && self.disposition == PrDisposition::Open
    }

    /// Whether the resolver failed to place this row.
    #[must_use]
    pub fn is_unresolved(&self) -> bool {
        self.verdict.needs_attention() && self.disposition == PrDisposition::Unknown
    }
}

/// Arithmetic proof of how much of the store this scan actually saw.
///
/// Every field is a count, not a flag, so the report can be checked by
/// subtraction rather than trusted. An empty actionable list is only good news
/// when `unresolved` is also zero; the renderer says so explicitly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerdictCensus {
    /// Ship records examined.
    pub scanned: usize,
    /// Records that had reached a terminal verdict.
    pub terminal: usize,
    /// Terminal records whose verdict passed.
    pub passed: usize,
    /// Terminal records whose verdict failed.
    pub failed: usize,
    /// Terminal records whose verdict was cancelled.
    pub cancelled: usize,
    /// Non-passing records whose pull request disposition was resolved.
    pub resolved: usize,
    /// Non-passing records whose pull request disposition could not be read.
    pub unresolved: usize,
    /// Non-passing records on a still-open pull request.
    pub actionable: usize,
}

impl VerdictCensus {
    /// Whether the verdict tally accounts for every terminal record.
    ///
    /// This is the instrument's self-check. If it is false the classifier
    /// dropped records on the floor and no other number in the census can be
    /// trusted, so the renderer refuses to report a clean result.
    #[must_use]
    pub fn is_self_consistent(&self) -> bool {
        self.passed + self.failed + self.cancelled == self.terminal && self.terminal <= self.scanned
    }

    /// Whether this scan can support a claim that nothing needs attention.
    ///
    /// Requires a consistent tally *and* zero unresolved rows. A scan that
    /// could not resolve anything reports nothing, rather than reporting
    /// "clean".
    #[must_use]
    pub fn supports_all_clear(&self) -> bool {
        self.is_self_consistent() && self.unresolved == 0 && self.actionable == 0
    }
}

/// A completed scan: the rows worth showing plus the census that bounds them.
#[derive(Debug, Clone, Default)]
pub struct VerdictReport {
    /// Non-passing rows, actionable first, then unresolved.
    pub rows: Vec<VerdictRow>,
    /// Counts describing everything the scan touched.
    pub census: VerdictCensus,
}

impl VerdictReport {
    /// Rows that still need someone to act.
    #[must_use]
    pub fn actionable(&self) -> Vec<&VerdictRow> {
        self.rows.iter().filter(|row| row.is_actionable()).collect()
    }

    /// Rows whose pull request disposition could not be read.
    #[must_use]
    pub fn unresolved(&self) -> Vec<&VerdictRow> {
        self.rows.iter().filter(|row| row.is_unresolved()).collect()
    }
}

/// Classify one ship record's terminal verdict, or `None` while in flight.
///
/// Passed and failed follow [`ship_terminal_verdict`] exactly, so this never
/// disagrees with `watch` or `auto-merge` about whether a ship is finished. The
/// single refinement is that a non-passing verdict whose required lanes include
/// a cancelled run is reported as [`TerminalVerdict::Cancelled`].
#[must_use]
pub fn terminal_verdict(state: &ShipState) -> Option<TerminalVerdict> {
    match ship_terminal_verdict(state) {
        None => None,
        Some(true) => Some(TerminalVerdict::Passed),
        Some(false) => {
            if has_cancelled_required_run(state) {
                Some(TerminalVerdict::Cancelled)
            } else {
                Some(TerminalVerdict::Failed)
            }
        }
    }
}

/// Whether any merge-blocking lane recorded a cancellation.
fn has_cancelled_required_run(state: &ShipState) -> bool {
    state
        .dispatched_runs
        .iter()
        .any(|run| run.required && run.status.eq_ignore_ascii_case(CANCELLED_RUN_STATUS))
}

/// Build a report from ship records and whatever dispositions were resolved.
///
/// `disposition_of` is passed in rather than fetched here so the reconciliation
/// rules stay testable without a network, and so the caller owns the batching
/// decision. A repository the caller could not read simply yields
/// [`PrDisposition::Unknown`], which lands in `unresolved`.
pub fn build_report<F>(states: &[ShipState], mut disposition_of: F) -> VerdictReport
where
    F: FnMut(&str, u64) -> PrDisposition,
{
    let mut census = VerdictCensus {
        scanned: states.len(),
        ..VerdictCensus::default()
    };
    let mut rows = Vec::new();

    for state in states {
        let Some(verdict) = terminal_verdict(state) else {
            continue;
        };
        census.terminal += 1;
        match verdict {
            TerminalVerdict::Passed => {
                census.passed += 1;
                continue;
            }
            TerminalVerdict::Failed => census.failed += 1,
            TerminalVerdict::Cancelled => census.cancelled += 1,
        }

        let disposition = disposition_of(&state.repo, state.pr);
        if disposition == PrDisposition::Unknown {
            census.unresolved += 1;
        } else {
            census.resolved += 1;
        }

        let row = VerdictRow {
            repo: state.repo.clone(),
            pr: state.pr,
            verdict,
            disposition,
            title: state.pr_title.clone(),
            url: state.pr_url.clone(),
        };
        if row.is_actionable() {
            census.actionable += 1;
        }
        rows.push(row);
    }

    rows.sort_by(|left, right| {
        rank(left)
            .cmp(&rank(right))
            .then_with(|| left.repo.cmp(&right.repo))
            .then_with(|| left.pr.cmp(&right.pr))
    });

    VerdictReport { rows, census }
}

/// Sort key: actionable rows first, then unresolved, then spent history.
fn rank(row: &VerdictRow) -> u8 {
    if row.is_actionable() {
        0
    } else if row.is_unresolved() {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{PrDisposition, TerminalVerdict, VerdictCensus, build_report, terminal_verdict};
    use crate::ship_state::{DispatchedRun, ShipState};

    fn sample_state(repo: &str, pr: u64) -> ShipState {
        let mut state = ShipState::new(
            pr,
            repo,
            format!("feature/{pr}"),
            "main",
            format!("{pr:0>40}"),
            "signature",
        );
        state.pr_url = format!("https://github.com/{repo}/pull/{pr}");
        state.pr_title = format!("change {pr}");
        state
    }

    fn sample_run(target: &str, status: &str) -> DispatchedRun {
        let now = Utc::now();
        DispatchedRun {
            target: target.to_owned(),
            provider: "local".to_owned(),
            run_id: format!("sy-{target}-{status}"),
            status: status.to_owned(),
            started_at: now,
            updated_at: now,
            attempt: 1,
            last_heartbeat_at: None,
            phase: None,
            required: true,
        }
    }

    /// A ship that concluded `fail` on a still-open pull request is the exact
    /// case nothing consumes today, so it must come back actionable.
    ///
    /// The CONTROL is the same verdict on a merged pull request. Without it a
    /// passing assertion here would not distinguish "reconciliation works" from
    /// "everything non-passing is reported", which is the 91%-noise failure
    /// that trains people to ignore a list.
    #[test]
    fn a_failed_verdict_is_actionable_only_while_its_pull_request_is_open() {
        let mut open = sample_state("acme/widget", 140);
        open.dispatched_runs.push(sample_run("mac", "failed"));
        open.evidence_snapshot
            .insert("mac".to_owned(), "fail".to_owned());

        let report = build_report(std::slice::from_ref(&open), |_, _| PrDisposition::Open);
        assert_eq!(report.census.actionable, 1);
        assert_eq!(report.actionable().len(), 1);
        assert_eq!(report.actionable()[0].pr, 140);

        // CONTROL: the identical record, resolved as merged, must drop out of
        // the actionable set while still being counted as terminal and failed.
        let merged = build_report(std::slice::from_ref(&open), |_, _| PrDisposition::Merged);
        assert_eq!(merged.census.actionable, 0);
        assert_eq!(merged.actionable().len(), 0);
        assert_eq!(merged.census.failed, 1, "the verdict itself is unchanged");
    }

    /// The crux. A resolver that cannot answer must make the scan read as
    /// *unresolved*, never as an empty actionable list, or this consumer
    /// reproduces the silent-failure bug it exists to close.
    #[test]
    fn a_resolver_that_answers_nothing_is_visibly_blind_not_quietly_clean() {
        let mut failed = sample_state("acme/widget", 141);
        failed.dispatched_runs.push(sample_run("mac", "failed"));
        failed
            .evidence_snapshot
            .insert("mac".to_owned(), "fail".to_owned());

        let blind = build_report(std::slice::from_ref(&failed), |_, _| PrDisposition::Unknown);
        assert_eq!(blind.census.actionable, 0, "nothing can be placed as open");
        assert_eq!(blind.census.unresolved, 1);
        assert_eq!(blind.unresolved().len(), 1);
        assert!(
            !blind.census.supports_all_clear(),
            "an unresolved row must forbid an all-clear claim"
        );

        // CONTROL: the same record with a working resolver does support a
        // conclusion. Without this the assertion above could pass simply
        // because `supports_all_clear` never returns true.
        let sighted = build_report(std::slice::from_ref(&failed), |_, _| PrDisposition::Merged);
        assert_eq!(sighted.census.unresolved, 0);
        assert!(sighted.census.supports_all_clear());
    }

    /// A cancelled lane asks to be re-run; a failed lane asks to be
    /// investigated. Reporting both as "failed" hides which happened.
    #[test]
    fn a_cancelled_required_lane_is_reported_as_cancelled_not_failed() {
        let mut cancelled = sample_state("acme/widget", 142);
        cancelled
            .dispatched_runs
            .push(sample_run("mac", "cancelled"));
        cancelled
            .evidence_snapshot
            .insert("mac".to_owned(), "fail".to_owned());
        assert_eq!(
            terminal_verdict(&cancelled),
            Some(TerminalVerdict::Cancelled)
        );

        // CONTROL: an ordinary failing lane on an otherwise identical record
        // still reports failed, so the branch above is not matching everything.
        let mut failed = sample_state("acme/widget", 143);
        failed.dispatched_runs.push(sample_run("mac", "failed"));
        failed
            .evidence_snapshot
            .insert("mac".to_owned(), "fail".to_owned());
        assert_eq!(terminal_verdict(&failed), Some(TerminalVerdict::Failed));
    }

    /// An in-flight ship must not be reported at all — a verdict that has not
    /// happened yet is exactly the "pushed and revalidating" state that was
    /// wrongly treated as terminal.
    #[test]
    fn a_ship_with_no_evidence_yet_is_not_given_a_verdict() {
        let in_flight = sample_state("acme/widget", 144);
        assert_eq!(terminal_verdict(&in_flight), None);

        let report = build_report(std::slice::from_ref(&in_flight), |_, _| {
            panic!("an in-flight record must never cost a disposition lookup")
        });
        assert_eq!(report.census.scanned, 1);
        assert_eq!(report.census.terminal, 0);
        assert!(report.rows.is_empty());
    }

    /// The census is checked by subtraction, not trusted. If the classifier
    /// ever drops a terminal record, this is what notices.
    #[test]
    fn the_census_accounts_for_every_terminal_record_it_counted() {
        let mut states = Vec::new();
        for (pr, target_status, evidence) in [
            (200_u64, "completed", "pass"),
            (201, "failed", "fail"),
            (202, "cancelled", "fail"),
        ] {
            let mut state = sample_state("acme/widget", pr);
            state.dispatched_runs.push(sample_run("mac", target_status));
            state
                .evidence_snapshot
                .insert("mac".to_owned(), evidence.to_owned());
            states.push(state);
        }
        let in_flight = sample_state("acme/widget", 203);
        states.push(in_flight);

        let report = build_report(&states, |_, _| PrDisposition::Open);
        let census = report.census;
        assert_eq!(census.scanned, 4);
        assert_eq!(census.terminal, 3, "the in-flight record is not terminal");
        assert_eq!(census.passed, 1);
        assert_eq!(census.failed, 1);
        assert_eq!(census.cancelled, 1);
        assert!(census.is_self_consistent());
        assert_eq!(
            census.actionable, 2,
            "the pass needs nobody; the fail and the cancellation do"
        );

        // CONTROL: a deliberately miscounted census must be rejected, proving
        // `is_self_consistent` can return false rather than always agreeing.
        let broken = VerdictCensus {
            terminal: 3,
            passed: 1,
            failed: 1,
            cancelled: 0,
            ..census
        };
        assert!(!broken.is_self_consistent());
    }

    /// Actionable rows must sort ahead of unresolved and spent ones, so the
    /// first thing printed is the thing to act on.
    #[test]
    fn actionable_rows_sort_ahead_of_unresolved_and_spent_rows() {
        let mut states = Vec::new();
        for pr in [300_u64, 301, 302] {
            let mut state = sample_state("acme/widget", pr);
            state.dispatched_runs.push(sample_run("mac", "failed"));
            state
                .evidence_snapshot
                .insert("mac".to_owned(), "fail".to_owned());
            states.push(state);
        }

        let report = build_report(&states, |_, pr| match pr {
            300 => PrDisposition::Merged,
            301 => PrDisposition::Unknown,
            _ => PrDisposition::Open,
        });
        let order = report.rows.iter().map(|row| row.pr).collect::<Vec<_>>();
        assert_eq!(order, vec![302, 301, 300]);
    }
}
