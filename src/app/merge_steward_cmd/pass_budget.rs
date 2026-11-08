//! Bound one steward pass in wall time.
//!
//! A pass reads GitHub once per open pull request it acts on, so its length
//! grows with the backlog. A scheduler that kills a pass at a fixed timeout
//! loses its whole report, and the next pass starts the same work from the
//! top and is killed in the same place. So past `--pass-budget-secs` (or once
//! GitHub says the token's rate limit is spent) a pass starts no new per-PR
//! work: each remaining row is reported `deferred_pass_budget`, with its
//! census decision but no mutation, and the next pass takes those rows first.
//!
//! Deferral only ever withholds work. A deferred row is never admitted,
//! armed, enqueued or dequeued, and the native-arm backstop skips it, so
//! nothing reaches the base without the full per-PR evaluation.

use std::collections::BTreeSet;
use std::time::Instant;

use super::blocked_on::Stall;
use super::next_action::NextAction;
use super::{ObservedPr, PrReport, StewardDecision, StewardLedger};

/// The `mutation` and `blocked_on` token of a deferred row.
pub(super) const DEFERRED: &str = "deferred_pass_budget";

/// Whether the pass may still start per-PR work.
pub(super) struct PassBudget {
    deadline: Option<Instant>,
    rate_limited: bool,
}

impl PassBudget {
    pub(super) const fn new(deadline: Option<Instant>) -> Self {
        Self {
            deadline,
            rate_limited: false,
        }
    }

    /// Why no new per-PR work may start, or `None` while it may.
    pub(super) fn exhausted(&self) -> Option<&'static str> {
        if self.rate_limited {
            Some("GitHub reported the API rate limit exceeded earlier in this pass")
        } else if deadline_passed(self.deadline) {
            Some("the pass time budget is spent")
        } else {
            None
        }
    }

    /// Note a finished row. A rate-limited read fails every later read of
    /// the pass alike, so the rest are deferred rather than spent on errors.
    pub(super) fn observe(&mut self, report: &PrReport) {
        if report.error.as_deref().is_some_and(is_rate_limited) {
            self.rate_limited = true;
        }
    }
}

/// Whether `deadline` has passed. `None` never passes.
pub(super) fn deadline_passed(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

fn is_rate_limited(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("api rate limit exceeded") || lower.contains("secondary rate limit")
}

/// The row for a pull request this pass did not evaluate.
pub(super) fn deferred_report(pr: &ObservedPr, decision: StewardDecision, why: &str) -> PrReport {
    PrReport {
        number: pr.fact.number,
        head_sha: pr.fact.head_sha.clone(),
        decision,
        mutation: Some(DEFERRED.to_owned()),
        error: None,
        blocked: Stall {
            blocked_on: DEFERRED.to_owned(),
            blocked_detail: Some(format!(
                "{why}; not evaluated or mutated this pass, taken first next pass"
            )),
            next_action: Some(NextAction::deferred_pass_budget()),
            ..Stall::default()
        },
    }
}

pub(super) fn is_deferred(report: &PrReport) -> bool {
    report.mutation.as_deref() == Some(DEFERRED)
}

/// The order a pass evaluates `prs` in, as indexes into `prs`: admission
/// candidates first, then queued or armed rows (the unapproved-admission
/// sweep), then rows the previous pass deferred, then the rest; census order
/// within each group. A row deferred once is therefore evaluated ahead of
/// rows that were not, so the same tail is not starved pass after pass.
pub(super) fn processing_order(
    repo: &str,
    prs: &[ObservedPr],
    decisions: &[StewardDecision],
    ledger: &StewardLedger,
) -> Vec<usize> {
    let carried: BTreeSet<u64> = ledger
        .pass_budget_deferred
        .get(&ledger_key(repo))
        .map(|numbers| numbers.iter().copied().collect())
        .unwrap_or_default();
    let mut order: Vec<usize> = (0..prs.len()).collect();
    order.sort_by_key(|&index| {
        let pr = &prs[index];
        let group = match decisions.get(index) {
            Some(StewardDecision::ArmMergeQueue | StewardDecision::RerunTransient { .. }) => 0,
            _ if pr.fact.queue_position.is_some() || pr.fact.auto_merge_active => 1,
            _ if carried.contains(&pr.fact.number) => 2,
            _ => 3,
        };
        (group, index)
    });
    order
}

/// Remember which rows this pass deferred, for the next pass's order.
pub(super) fn record_deferred(ledger: &mut StewardLedger, repo: &str, reports: &[PrReport]) {
    let deferred: Vec<u64> = reports
        .iter()
        .filter(|report| is_deferred(report))
        .map(|report| report.number)
        .collect();
    if deferred.is_empty() {
        ledger.pass_budget_deferred.remove(&ledger_key(repo));
    } else {
        ledger
            .pass_budget_deferred
            .insert(ledger_key(repo), deferred);
    }
}

fn ledger_key(repo: &str) -> String {
    repo.to_ascii_lowercase()
}
