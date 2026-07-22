//! Merge-queue enqueue / poll / eviction engine.
//!
//! When a repository is governed by GitHub's merge queue, a shipped PR is not
//! merged the moment its required checks go green — it is *enqueued*, and the
//! queue runs the required suite again against the speculative merge before it
//! lands. Shipyard supervises that window: it enqueues the PR, polls the queue
//! for the PR's standing, and re-enqueues if the PR is evicted (a sibling ahead
//! of it failed and GitHub dropped the speculative batch).
//!
//! The classification below is deliberately pure: it turns a parsed GraphQL
//! poll response plus timing context into a [`QueuePollClass`], so the poll
//! driver stays a thin loop and every edge is unit-testable without a live
//! queue or any GitHub API call.

use std::time::Duration;

use serde::Serialize;

/// Number of entries requested per merge-queue `entries` page.
///
/// GitHub caps a connection page at 100 nodes.
pub const QUEUE_PAGE_SIZE: usize = 100;

/// The target PR's standing in the merge queue for a single poll.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueSnapshot {
    /// Whether the target PR appeared among the returned queue entries.
    pub pr_found: bool,
    /// The PR's zero-based position in the queue, when found.
    pub position: Option<u64>,
    /// Number of entries returned by this response.
    pub entries_returned: usize,
    /// Whether the queue connection reported more entries beyond what was read
    /// (`pageInfo.hasNextPage`). When true the PR's absence is unproven: it may
    /// simply live on a page we did not fetch.
    pub page_truncated: bool,
    /// The queue's reported total entry count, when available.
    pub total_entries: Option<u64>,
}

/// Timing and history context for classifying one poll.
#[derive(Clone, Debug)]
pub struct PollContext {
    /// Wall-clock time elapsed since the current enqueue attempt started.
    pub attempt_elapsed: Duration,
    /// Minimum time that must elapse after an enqueue before an absence is
    /// trusted as an eviction.
    pub settle_window: Duration,
    /// Whether the PR has been observed in the queue during this attempt.
    pub seen_in_queue: bool,
    /// Consecutive errored polls observed so far in this attempt.
    pub consecutive_errors: u32,
    /// Maximum consecutive errored polls tolerated before degrading.
    pub error_budget: u32,
}

/// Classification of a single merge-queue poll.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "class", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueuePollClass {
    /// PR is in the queue; keep supervising.
    Enqueued {
        /// The PR's position, when the response reported it.
        position: Option<u64>,
    },
    /// PR was in the queue and has been dropped; re-enqueue it.
    Evicted,
    /// PR is provably absent from a valid queue response and was never seen;
    /// supervision ends.
    PrNotFound,
    /// The attempt ran out of actionable signal; a non-terminal outcome the
    /// caller can retry or surface.
    TimedOut,
}

impl QueuePollClass {
    /// Whether this classification ends supervision.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::PrNotFound)
    }
}

/// Parse a merge-queue GraphQL poll response into a [`QueueSnapshot`].
///
/// Walks `data.repository.mergeQueue.entries`, locating `pr_number` among the
/// entry nodes.
#[must_use]
pub fn parse_queue_snapshot(body: &serde_json::Value, pr_number: u64) -> QueueSnapshot {
    let entries = body
        .get("data")
        .and_then(|data| data.get("repository"))
        .and_then(|repo| repo.get("mergeQueue"))
        .and_then(|queue| queue.get("entries"));

    let nodes = entries
        .and_then(|entries| entries.get("nodes"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut pr_found = false;
    let mut position = None;
    for node in &nodes {
        let node_pr = node
            .get("pullRequest")
            .and_then(|pr| pr.get("number"))
            .and_then(serde_json::Value::as_u64);
        if node_pr == Some(pr_number) {
            pr_found = true;
            position = node.get("position").and_then(serde_json::Value::as_u64);
            break;
        }
    }

    let page_truncated = entries
        .and_then(|entries| entries.get("pageInfo"))
        .and_then(|info| info.get("hasNextPage"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let total_entries = entries
        .and_then(|entries| entries.get("totalEntries"))
        .and_then(serde_json::Value::as_u64);

    QueueSnapshot {
        pr_found,
        position,
        entries_returned: nodes.len(),
        page_truncated,
        total_entries,
    }
}

/// Classify a single merge-queue poll.
#[must_use]
pub fn classify_poll(snap: &QueueSnapshot, ctx: &PollContext) -> QueuePollClass {
    if snap.pr_found {
        return QueuePollClass::Enqueued {
            position: snap.position,
        };
    }

    if ctx.seen_in_queue {
        QueuePollClass::Evicted
    } else {
        QueuePollClass::PrNotFound
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_with_entries(prs: &[u64], has_next_page: bool) -> serde_json::Value {
        let nodes = prs
            .iter()
            .enumerate()
            .map(|(idx, pr)| {
                serde_json::json!({
                    "pullRequest": { "number": pr },
                    "position": idx as u64,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "data": {
                "repository": {
                    "mergeQueue": {
                        "entries": {
                            "nodes": nodes,
                            "pageInfo": { "hasNextPage": has_next_page },
                            "totalEntries": prs.len(),
                        }
                    }
                }
            }
        })
    }

    fn ctx(seen: bool) -> PollContext {
        PollContext {
            attempt_elapsed: Duration::from_secs(60),
            settle_window: Duration::from_secs(10),
            seen_in_queue: seen,
            consecutive_errors: 0,
            error_budget: 5,
        }
    }

    #[test]
    fn present_pr_is_enqueued() {
        let body = body_with_entries(&[7, 42, 9], false);
        let snap = parse_queue_snapshot(&body, 42);
        assert!(snap.pr_found);
        assert_eq!(snap.position, Some(1));
        assert_eq!(
            classify_poll(&snap, &ctx(true)),
            QueuePollClass::Enqueued { position: Some(1) }
        );
    }

    #[test]
    fn absent_and_never_seen_is_pr_not_found() {
        let body = body_with_entries(&[7, 9], false);
        let snap = parse_queue_snapshot(&body, 42);
        assert!(!snap.pr_found);
        assert_eq!(classify_poll(&snap, &ctx(false)), QueuePollClass::PrNotFound);
    }

    #[test]
    fn absent_after_seen_in_full_queue_is_evicted() {
        let body = body_with_entries(&[7, 9], false);
        let snap = parse_queue_snapshot(&body, 42);
        assert!(!snap.pr_found);
        assert_eq!(classify_poll(&snap, &ctx(true)), QueuePollClass::Evicted);
    }
}
