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
        if report.error.as_deref().is_some_and(is_rate_limited)
            || report.mutation.as_deref().is_some_and(is_rate_limited)
        {
            self.rate_limited = true;
        }
    }
}

/// Whether `deadline` has passed. `None` never passes.
pub(super) fn deadline_passed(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

/// Whether `error` is GitHub refusing for rate: the primary REST limit
/// ("API rate limit exceeded for installation ID ...", "... already exceeded
/// for user ID ..."), GraphQL's `RATE_LIMITED`, the secondary or abuse limit,
/// or an HTTP 429.
pub(super) fn is_rate_limited(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "rate limit exceeded",
        "rate limit already exceeded",
        "rate_limited",
        "secondary rate limit",
        "abuse detection",
        "http 429",
        "too many requests",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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

/// The order a pass evaluates `prs` in, as indexes into `prs`: queued or
/// armed rows first (the unapproved-admission sweep), then admission
/// candidates, then rows the previous pass deferred, then the rest; census order
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
            // Queued or armed rows first: their unapproved-admission sweep
            // withdraws what the gate does not admit, and deferring it would
            // leave such an entry free to merge.
            _ if pr.fact.queue_position.is_some() || pr.fact.auto_merge_active => 0,
            Some(StewardDecision::ArmMergeQueue | StewardDecision::RerunTransient { .. }) => 1,
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::{StewardDecision, StewardLedger, parse_pr};
    use super::{is_rate_limited, processing_order};

    fn pr(number: u64, positions: &BTreeMap<u64, u64>) -> super::ObservedPr {
        parse_pr(
            &serde_json::json!({
                "id": format!("PR_{number}"),
                "number": number,
                "isDraft": false,
                "headRefOid": format!("{number:040x}"),
                "headRefName": format!("feature/{number}"),
                "mergeStateStatus": "CLEAN",
                "autoMergeRequest": null,
                "labels": [{"name": "shipyard:managed"}],
                "statusCheckRollup": []
            }),
            positions,
        )
        .expect("PR")
    }

    /// Queued rows first (their unapproved-admission sweep must never be
    /// deferred), then admission candidates, then last pass's deferred rows,
    /// then the rest, census order within each.
    #[test]
    fn queued_rows_then_ready_rows_then_last_pass_deferred_rows_go_first() {
        let positions = BTreeMap::from([(4, 1)]);
        let prs: Vec<_> = [1, 2, 3, 4, 5].iter().map(|n| pr(*n, &positions)).collect();
        let other = StewardDecision::Unmanaged;
        let decisions = vec![
            other.clone(),
            other.clone(),
            StewardDecision::ArmMergeQueue,
            other.clone(),
            other,
        ];
        let mut ledger = StewardLedger::default();
        ledger
            .pass_budget_deferred
            .insert("owner/repo".to_owned(), vec![5]);
        let order: Vec<u64> = processing_order("Owner/Repo", &prs, &decisions, &ledger)
            .into_iter()
            .map(|index| prs[index].fact.number)
            .collect();
        assert_eq!(order, vec![4, 3, 5, 1, 2]);
    }

    #[test]
    fn github_rate_limit_refusals_are_recognised() {
        for message in [
            "gh: API rate limit exceeded for installation ID 147677577. (HTTP 403)",
            "API rate limit already exceeded for user ID 1.",
            r#"{"errors":[{"type":"RATE_LIMITED","message":"API rate limit exceeded"}]}"#,
            "You have exceeded a secondary rate limit. Please wait a few minutes",
            "You have triggered an abuse detection mechanism.",
            "gh: Too Many Requests (HTTP 429)",
        ] {
            assert!(is_rate_limited(message), "{message}");
        }
    }

    #[test]
    fn other_failures_are_not_rate_limits() {
        for message in [
            "gh: Not Found (HTTP 404)",
            "gh: Resource not accessible by integration (HTTP 403)",
            "HTTP 502 Bad Gateway",
            "the rate of change is unknown",
        ] {
            assert!(!is_rate_limited(message), "{message}");
        }
    }
}
