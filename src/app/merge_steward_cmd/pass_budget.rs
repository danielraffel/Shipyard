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

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Instant;

use super::blocked_on::Stall;
use super::next_action::NextAction;
use super::{ObservedPr, PrReport, StewardDecision, StewardLedger};

/// The `mutation` and `blocked_on` token of a deferred row.
pub(super) const DEFERRED: &str = "deferred_pass_budget";
/// Do not immediately retry a token that GitHub just rejected.  The floor is
/// deliberately short enough for a secondary limit while preventing a tight
/// scheduled-steward loop when the provider omits reset metadata.
pub(super) const RATE_LIMIT_COOLDOWN_FLOOR_SECONDS: i64 = 30;
const RATE_LIMIT_COOLDOWN_DEFAULT_SECONDS: i64 = 300;
const RATE_LIMIT_COOLDOWN_MAX_SECONDS: i64 = 24 * 60 * 60;

/// Durable repository-scoped rate-limit deferral.  `scope_key` is a hash of
/// the effective installation/token identity and never contains credential
/// material; the repository is included in the map key as a second fence.
#[derive(Clone, Debug, Default, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
pub(super) struct RateLimitCooldown {
    pub(super) repo: String,
    pub(super) scope_key: String,
    pub(super) until_epoch: i64,
    pub(super) observed_at_epoch: i64,
    pub(super) detail: String,
}

pub(super) fn rate_limit_cooldown_key(scope_key: &str, repo: &str) -> String {
    format!("{scope_key}:{}", repo.to_ascii_lowercase())
}

/// Return an active cooldown, if any.  Expired entries are left for the
/// caller to remove and persist so expiry itself is durable.
pub(super) fn active_rate_limit_cooldown<'a>(
    cooldowns: &'a BTreeMap<String, RateLimitCooldown>,
    key: &str,
    now_epoch: i64,
) -> Option<&'a RateLimitCooldown> {
    cooldowns
        .get(key)
        .filter(|cooldown| cooldown.until_epoch > now_epoch)
}

/// Compute the next permitted probe time from provider metadata.  GitHub's
/// CLI normally preserves the 403 text but not response headers; test/fleet
/// wrappers may include `Retry-After` or `X-RateLimit-Reset`, both of which
/// are accepted here.  Malformed or absent metadata falls back to a bounded
/// five-minute pause, and every value honours the anti-tight-loop floor.
pub(super) fn rate_limit_cooldown_until(error: &str, now_epoch: i64) -> i64 {
    let retry_after = header_number(error, &["retry-after"]);
    let reset_epoch = header_number(
        error,
        &[
            "x-ratelimit-reset",
            "x-rate-limit-reset",
            "rate_limit_reset",
            "reset_at_epoch",
        ],
    );
    let seconds = retry_after.or_else(|| reset_epoch.map(|reset| reset.saturating_sub(now_epoch)));
    now_epoch.saturating_add(
        seconds
            .unwrap_or(RATE_LIMIT_COOLDOWN_DEFAULT_SECONDS)
            .clamp(
                RATE_LIMIT_COOLDOWN_FLOOR_SECONDS,
                RATE_LIMIT_COOLDOWN_MAX_SECONDS,
            ),
    )
}

fn header_number(message: &str, names: &[&str]) -> Option<i64> {
    let lower = message.to_ascii_lowercase();
    for name in names {
        let mut offset = 0;
        while let Some(found) = lower[offset..].find(name) {
            let start = offset + found + name.len();
            let tail = message.get(start..)?;
            let digits = tail
                .chars()
                .skip_while(|character| !character.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            if let Ok(value) = digits.parse::<i64>() {
                return Some(value);
            }
            offset = start;
        }
    }
    None
}

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
/// candidates (rows the steward can enqueue or rerun), then signaling-only
/// work. Within the last two groups, rows the previous pass deferred go
/// first, oldest deferral first (the order `record_deferred` persisted), and
/// census order breaks the remaining ties.
///
/// The carry order matters as much as the carry itself. Census order is
/// newest first, so ordering carried rows by census index alone took the same
/// newest few every pass; the rows that pass did evaluate then rejoined the
/// carried set as newly deferred and were again the newest, so the oldest
/// tail was deferred forever.
pub(super) fn processing_order(
    repo: &str,
    prs: &[ObservedPr],
    decisions: &[StewardDecision],
    ledger: &StewardLedger,
) -> Vec<usize> {
    let carried: BTreeMap<u64, usize> = ledger
        .pass_budget_deferred
        .get(&ledger_key(repo))
        .map(|numbers| {
            numbers
                .iter()
                .enumerate()
                .map(|(rank, number)| (*number, rank))
                .collect()
        })
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
            _ => 2,
        };
        let carry_rank = carried.get(&pr.fact.number).copied().unwrap_or(usize::MAX);
        (group, carry_rank, index)
    });
    order
}

/// Remember which rows this pass deferred, in the order the next pass should
/// take them: rows the previous pass had already deferred keep their place at
/// the front, and rows deferred for the first time follow in census order. A
/// row evaluated this pass and deferred next pass therefore goes to the back,
/// so every deferred row reaches the front within a bounded number of passes.
pub(super) fn record_deferred(ledger: &mut StewardLedger, repo: &str, reports: &[PrReport]) {
    let deferred: BTreeSet<u64> = reports
        .iter()
        .filter(|report| is_deferred(report))
        .map(|report| report.number)
        .collect();
    if deferred.is_empty() {
        ledger.pass_budget_deferred.remove(&ledger_key(repo));
        return;
    }
    let previous = ledger
        .pass_budget_deferred
        .get(&ledger_key(repo))
        .cloned()
        .unwrap_or_default();
    let mut ordered: Vec<u64> = previous
        .iter()
        .copied()
        .filter(|number| deferred.contains(number))
        .collect();
    let mut seen: BTreeSet<u64> = ordered.iter().copied().collect();
    for report in reports {
        if deferred.contains(&report.number) && seen.insert(report.number) {
            ordered.push(report.number);
        }
    }
    ledger
        .pass_budget_deferred
        .insert(ledger_key(repo), ordered);
}

fn ledger_key(repo: &str) -> String {
    repo.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::{StewardDecision, StewardLedger, parse_pr};
    use super::{
        RATE_LIMIT_COOLDOWN_FLOOR_SECONDS, RateLimitCooldown, active_rate_limit_cooldown,
        deferred_report, is_rate_limited, processing_order, rate_limit_cooldown_key,
        rate_limit_cooldown_until, record_deferred,
    };

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

    /// Run steward passes over `prs` (census order) that each evaluate only
    /// `per_pass` rows, returning the numbers each pass evaluated.
    fn budgeted_passes(
        prs: &[super::ObservedPr],
        decisions: &[StewardDecision],
        per_pass: usize,
        passes: usize,
    ) -> Vec<Vec<u64>> {
        let mut ledger = StewardLedger::default();
        let mut evaluated = Vec::new();
        for _ in 0..passes {
            let order = processing_order("Owner/Repo", prs, decisions, &ledger);
            let taken: Vec<usize> = order.iter().copied().take(per_pass).collect();
            let reports: Vec<_> = (0..prs.len())
                .map(|index| {
                    let mut report =
                        deferred_report(&prs[index], decisions[index].clone(), "spent");
                    if taken.contains(&index) {
                        report.mutation = None;
                    }
                    report
                })
                .collect();
            record_deferred(&mut ledger, "Owner/Repo", &reports);
            evaluated.push(taken.iter().map(|&index| prs[index].fact.number).collect());
        }
        evaluated
    }

    /// A budget that covers only part of the backlog must still reach every
    /// row: the rows the last pass deferred go first, oldest deferral first,
    /// rather than the newest rows of the census every pass.
    #[test]
    fn deferred_rows_are_taken_first_oldest_deferral_first() {
        let positions = BTreeMap::new();
        // Census order is newest first.
        let prs: Vec<_> = [16, 15, 14, 13, 12, 11]
            .iter()
            .map(|n| pr(*n, &positions))
            .collect();
        let decisions = vec![StewardDecision::Unmanaged; prs.len()];
        let evaluated = budgeted_passes(&prs, &decisions, 2, 3);
        assert_eq!(evaluated[0], vec![16, 15]);
        assert_eq!(
            evaluated[1],
            vec![14, 13],
            "the next pass takes the deferred rows first"
        );
        assert_eq!(
            evaluated[2],
            vec![12, 11],
            "rows deferred longest go ahead of rows evaluated a pass ago"
        );
    }

    /// Enqueueable rows are evaluated before signaling-only rows, and a
    /// deferred enqueueable row goes ahead of a fresh one.
    #[test]
    fn enqueueable_rows_go_before_signaling_rows_and_carried_before_fresh() {
        let positions = BTreeMap::new();
        let prs: Vec<_> = [6, 5, 4, 3, 2, 1]
            .iter()
            .map(|n| pr(*n, &positions))
            .collect();
        let decisions = vec![
            StewardDecision::Unmanaged,
            StewardDecision::ArmMergeQueue,
            StewardDecision::Unmanaged,
            StewardDecision::ArmMergeQueue,
            StewardDecision::Unmanaged,
            StewardDecision::ArmMergeQueue,
        ];
        let mut ledger = StewardLedger::default();
        ledger
            .pass_budget_deferred
            .insert("owner/repo".to_owned(), vec![2, 1, 4]);
        let order: Vec<u64> = processing_order("Owner/Repo", &prs, &decisions, &ledger)
            .into_iter()
            .map(|index| prs[index].fact.number)
            .collect();
        assert_eq!(order, vec![1, 5, 3, 2, 4, 6]);
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

    #[test]
    fn rate_limit_403_honours_retry_after_and_reset_floor() {
        let now = 1_700_000_000;
        assert_eq!(
            rate_limit_cooldown_until(
                "gh: API rate limit exceeded (HTTP 403) Retry-After: 90",
                now
            ),
            now + 90
        );
        assert_eq!(
            rate_limit_cooldown_until(
                "gh: API rate limit exceeded (HTTP 403) X-RateLimit-Reset: 1700000120",
                now
            ),
            now + 120
        );
        assert_eq!(
            rate_limit_cooldown_until(
                "gh: API rate limit exceeded for installation ID 7 (HTTP 403) Retry-After: 0",
                now
            ),
            now + RATE_LIMIT_COOLDOWN_FLOOR_SECONDS
        );
    }

    #[test]
    fn expired_rate_limit_cooldown_is_probeable_again() {
        let key = rate_limit_cooldown_key("installation-7", "Owner/Repo");
        let cooldowns = BTreeMap::from([(
            key.clone(),
            RateLimitCooldown {
                repo: "Owner/Repo".to_owned(),
                scope_key: "installation-7".to_owned(),
                until_epoch: 1_700_000_010,
                observed_at_epoch: 1_700_000_000,
                detail: "HTTP 403 rate limit".to_owned(),
            },
        )]);
        assert!(active_rate_limit_cooldown(&cooldowns, &key, 1_700_000_009).is_some());
        assert!(active_rate_limit_cooldown(&cooldowns, &key, 1_700_000_010).is_none());
    }
}
