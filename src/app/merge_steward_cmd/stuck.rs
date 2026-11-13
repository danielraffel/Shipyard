//! Stuck pull requests: red on a required context, or held at the head, with
//! nothing pushed for a while.
//!
//! On 2026-11-13 Generous-Corp/vellum#397 sat 4 h 30 min red with nobody
//! told. The steward already knows each row's state from required contexts
//! only (`blocked_on`, see [`super::blocked_on`]: a context counts when the
//! base's rulesets or branch protection require it, from the app the rule
//! names) and when it began (the dwell entry, [`super::dwell_pass`], which a
//! new head starts over). This module turns that into one signal on the row:
//!
//! * `stuck_since`: when the state began at this head;
//! * `next_action.escalate`: `owner` after [`crate::merge_steward::stuck::OWNER_MINUTES`], `repair` after
//!   [`crate::merge_steward::stuck::REPAIR_MINUTES`].
//!
//! The steward only publishes. `shipyard relay` delivers it (to the owning
//! session, then the repair lane) and holds an escalation back while a repair
//! claim newer than `stuck_since` is fresh, because the relay is what reads
//! the pull request's comments; the steward reads none for this.
//!
//! Only `required_failed:<contexts>` and `held:<context>` rows can be stuck.
//! A pull request in the merge queue reads `queued` (its group is the
//! queue's to judge), so it is never stuck, whatever a non-required check on
//! its head says. A `required_failed` the steward is itself re-running as an
//! infrastructure interruption is not stuck either. Reads: none.

use chrono::{DateTime, Utc};

use super::PrReport;
use crate::merge_steward::StewardDecision;
use crate::merge_steward::dwell::{DwellRepoState, DwellState};

use crate::merge_steward::stuck::Escalate;

/// Whether a row's state can be stuck: a red required context or a HOLD at
/// the head, and never a merge-queue entry.
fn stuck_state(report: &PrReport) -> bool {
    if matches!(report.decision, StewardDecision::Queued { .. }) {
        return false;
    }
    let token = report.blocked.blocked_on.as_str();
    let kind = token.split_once(':').map_or(token, |(kind, _)| kind);
    matches!(kind, "required_failed" | "held")
}

/// Fill `stuck_since` and `next_action.escalate` on every stuck row.
/// `dwell` is this repository's dwell state after this pass.
pub(super) fn annotate(
    reports: &mut [PrReport],
    dwell: Option<&DwellRepoState>,
    now: DateTime<Utc>,
) {
    for report in reports.iter_mut() {
        if !stuck_state(report) {
            continue;
        }
        let entry = dwell
            .and_then(|dwell| dwell.entries.get(&report.number))
            .filter(|entry| entry.head.eq_ignore_ascii_case(&report.head_sha));
        // The steward is re-running an infrastructure interruption: not the
        // author's to move yet.
        if entry.is_some_and(|entry| entry.state == DwellState::RequiredFailedInfra) {
            continue;
        }
        // The dwell entry is head-scoped (a push starts it over) and
        // survives ticks; the row's own `since` dates it when dwell has none.
        let since = entry.map(|entry| entry.entered_at).or_else(|| {
            report
                .blocked
                .since
                .as_deref()
                .and_then(|since| DateTime::parse_from_rfc3339(since).ok())
                .map(|since| since.with_timezone(&Utc))
        });
        let Some(since) = since else {
            continue;
        };
        report.blocked.stuck_since = Some(since.to_rfc3339());
        let level = Escalate::for_minutes((now - since).num_minutes());
        if let Some(next) = report.blocked.next_action.as_mut() {
            next.escalate = level;
        }
    }
}
