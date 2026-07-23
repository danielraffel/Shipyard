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

/// Minimum time after an enqueue before a momentary absence is trusted as an
/// eviction.
///
/// GitHub's merge-queue `entries` connection is eventually consistent: it lags
/// the enqueue mutation by a few seconds, so the first poll right after
/// (re-)enqueue can read a just-armed PR as absent. A 10s settle window
/// comfortably covers that arm-write lag while costing at most one extra poll
/// before a genuine eviction is actioned.
pub const DEFAULT_SETTLE_WINDOW: Duration = Duration::from_secs(10);

/// Consecutive errored polls tolerated before a poll attempt degrades from
/// retry-in-place to a non-terminal [`QueuePollClass::TimedOut`].
///
/// Merge-queue polls run on a ~15-30s cadence; the shared GitHub App token
/// rides a secondary rate limit that produces short, self-clearing error
/// bursts. A budget of five consecutive errors rides out roughly a minute of
/// sustained blips (≈100s at a 20s cadence) before giving up — long enough to
/// survive a transient secondary-limit window, short enough that a genuine
/// outage surfaces as an actionable `TimedOut` within about two minutes rather
/// than polling blind forever. Critically, an errored body NEVER degrades to
/// the terminal `PrNotFound`: a rate-limit blip must not report the PR gone.
pub const DEFAULT_ERROR_BUDGET: u32 = 5;

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
    /// The poll response was errored / null / malformed (rate-limit, transient
    /// blip) and the error budget is not yet spent; retry the poll in place.
    /// Non-terminal, and never conflated with a genuine absence.
    PollError {
        /// Human-readable reason drawn from the errored body.
        reason: String,
    },
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

/// Outcome of parsing a merge-queue GraphQL poll response.
///
/// Distinguishing an *errored / null* body from a *valid response in which the
/// PR is simply absent* is the whole point: the former must retry, the latter
/// may be terminal. Collapsing both to "PR not found" is what strands a
/// green-outside-the-queue PR on a single rate-limit blip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueuePollParse {
    /// A well-formed queue response.
    Valid(QueueSnapshot),
    /// A null-data, GraphQL-errors, or structurally-missing response that
    /// carries no trustworthy statement about the PR's queue standing.
    Errored(String),
}

/// PR-level facts returned alongside each sparse queue poll.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuePrObservation {
    /// Current full head SHA.
    pub head_sha: String,
    /// Whether GitHub reports the PR merged.
    pub merged: bool,
    /// Latest queue-removal reason, when one exists.
    pub removal_reason: Option<String>,
    /// Creation time of the latest removal event.
    pub removal_at: Option<String>,
}

/// Parse the target PR facts included in the queue GraphQL response.
pub fn parse_pr_observation(body: &serde_json::Value) -> Result<QueuePrObservation, String> {
    if body
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        return Err("graphql errors present".to_owned());
    }
    let pr = body
        .pointer("/data/repository/pullRequest")
        .filter(|value| !value.is_null())
        .ok_or_else(|| "response missing pull request".to_owned())?;
    let head_sha = pr
        .get("headRefOid")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "response missing pull request head SHA".to_owned())?
        .to_owned();
    let merged = pr
        .get("merged")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "response missing pull request merged state".to_owned())?;
    let removal_reason = pr
        .pointer("/timelineItems/nodes")
        .and_then(serde_json::Value::as_array)
        .and_then(|nodes| nodes.last())
        .and_then(|node| node.get("reason"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let removal_at = pr
        .pointer("/timelineItems/nodes")
        .and_then(serde_json::Value::as_array)
        .and_then(|nodes| nodes.last())
        .and_then(|node| node.get("createdAt"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok(QueuePrObservation {
        head_sha,
        merged,
        removal_reason,
        removal_at,
    })
}

/// Parse the evaluated branch-rules response used to choose the merge path.
///
/// The endpoint returns an array of rule objects. A malformed response is an
/// error rather than "no queue": falling back to a direct merge when
/// governance could not be read would reintroduce dual merge authorities.
pub fn rules_require_merge_queue(body: &serde_json::Value) -> Result<bool, String> {
    let rules = body
        .as_array()
        .ok_or_else(|| "evaluated branch rules response is not an array".to_owned())?;
    Ok(rules.iter().any(|rule| {
        rule.get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind == "merge_queue")
    }))
}

/// Parse a merge-queue GraphQL poll response into a [`QueuePollParse`].
///
/// A body is [`QueuePollParse::Errored`] when it carries GraphQL `errors`, a
/// null top-level `data`, a null `repository` (unresolvable / permission /
/// transient), or a structurally-absent merge-queue `entries` connection. Only
/// a well-formed `entries` connection yields a [`QueuePollParse::Valid`]
/// snapshot in which the PR's absence is a trustworthy statement.
#[must_use]
pub fn parse_queue_snapshot(body: &serde_json::Value, pr_number: u64) -> QueuePollParse {
    if let Some(errors) = body.get("errors").and_then(serde_json::Value::as_array)
        && !errors.is_empty()
    {
        let reason = errors
            .iter()
            .filter_map(|err| err.get("message").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("; ");
        let reason = if reason.is_empty() {
            "graphql errors present".to_owned()
        } else {
            reason
        };
        return QueuePollParse::Errored(reason);
    }

    let Some(data) = body.get("data") else {
        return QueuePollParse::Errored("response missing `data`".to_owned());
    };
    if data.is_null() {
        return QueuePollParse::Errored("response `data` is null".to_owned());
    }

    let Some(repository) = data.get("repository") else {
        return QueuePollParse::Errored("response missing `repository`".to_owned());
    };
    if repository.is_null() {
        return QueuePollParse::Errored("response `repository` is null".to_owned());
    }

    let entries = repository
        .get("mergeQueue")
        .filter(|queue| !queue.is_null())
        .and_then(|queue| queue.get("entries"))
        .filter(|entries| !entries.is_null());

    let Some(entries) = entries else {
        return QueuePollParse::Errored("response missing merge-queue entries".to_owned());
    };

    let Some(nodes) = entries
        .get("nodes")
        .and_then(serde_json::Value::as_array)
        .cloned()
    else {
        return QueuePollParse::Errored("response merge-queue entries missing `nodes`".to_owned());
    };

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

    let Some(page_truncated) = entries
        .get("pageInfo")
        .and_then(|info| info.get("hasNextPage"))
        .and_then(serde_json::Value::as_bool)
    else {
        return QueuePollParse::Errored(
            "response merge-queue entries missing `pageInfo.hasNextPage`".to_owned(),
        );
    };

    let total_entries = entries
        .get("totalEntries")
        .and_then(serde_json::Value::as_u64);

    QueuePollParse::Valid(QueueSnapshot {
        pr_found,
        position,
        entries_returned: nodes.len(),
        page_truncated,
        total_entries,
    })
}

/// Classify a single merge-queue poll.
///
/// An errored / null body is retried in place ([`QueuePollClass::PollError`])
/// until the consecutive-error budget is spent, at which point it degrades to
/// the non-terminal [`QueuePollClass::TimedOut`]. It is never conflated with a
/// genuine absence: only a *valid* response with the PR missing can reach
/// [`QueuePollClass::PrNotFound`].
#[must_use]
pub fn classify_poll(parse: &QueuePollParse, ctx: &PollContext) -> QueuePollClass {
    let snap = match parse {
        QueuePollParse::Errored(reason) => {
            if ctx.consecutive_errors >= ctx.error_budget {
                return QueuePollClass::TimedOut;
            }
            return QueuePollClass::PollError {
                reason: reason.clone(),
            };
        }
        QueuePollParse::Valid(snap) => snap,
    };

    if snap.pr_found {
        return QueuePollClass::Enqueued {
            position: snap.position,
        };
    }

    // pr absent from what we read. If the queue reported more entries than we
    // fetched, the PR may simply live on an unread page (position 101+). Absence
    // is unproven, so neither an eviction nor a terminal not-found can be
    // concluded — keep supervising and let the driver paginate the full queue.
    if snap.page_truncated {
        return QueuePollClass::Enqueued { position: None };
    }

    // The entries connection lags the enqueue mutation. Within the settle
    // window after the initial enqueue or a re-enqueue, a momentary absence is
    // arm-write lag, not a trustworthy not-found / eviction verdict. This
    // applies before the PR has ever been observed too.
    if ctx.attempt_elapsed < ctx.settle_window {
        return QueuePollClass::Enqueued { position: None };
    }

    if ctx.seen_in_queue {
        QueuePollClass::Evicted
    } else {
        QueuePollClass::PrNotFound
    }
}

/// Fold a full set of merge-queue `entries` pages into a single
/// [`QueuePollParse`].
///
/// The poll driver paginates the queue with `entries(first:100, after:cursor)`
/// and hands every page here so absence is judged against the *whole* queue,
/// not a truncated first page. Any errored page taints the scan — absence
/// cannot be proven through a hole — so the first [`QueuePollParse::Errored`]
/// is returned. The aggregate `page_truncated` reflects the final page's
/// `hasNextPage`: it is false only when pagination genuinely drained the queue.
#[must_use]
pub fn parse_queue_pages(pages: &[serde_json::Value], pr_number: u64) -> QueuePollParse {
    let mut pr_found = false;
    let mut position = None;
    let mut entries_returned = 0;
    let mut total_entries = None;
    let mut page_truncated = false;

    for page in pages {
        match parse_queue_snapshot(page, pr_number) {
            QueuePollParse::Errored(reason) => return QueuePollParse::Errored(reason),
            QueuePollParse::Valid(snap) => {
                if snap.pr_found && !pr_found {
                    pr_found = true;
                    position = snap.position;
                }
                entries_returned += snap.entries_returned;
                if snap.total_entries.is_some() {
                    total_entries = snap.total_entries;
                }
                // Only the last page's continuation matters: an earlier page
                // always has a next page by construction.
                page_truncated = snap.page_truncated;
            }
        }
    }

    QueuePollParse::Valid(QueueSnapshot {
        pr_found,
        position,
        entries_returned,
        page_truncated,
        total_entries,
    })
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
            settle_window: DEFAULT_SETTLE_WINDOW,
            seen_in_queue: seen,
            consecutive_errors: 0,
            error_budget: DEFAULT_ERROR_BUDGET,
        }
    }

    fn expect_valid(parse: &QueuePollParse) -> &QueueSnapshot {
        match parse {
            QueuePollParse::Valid(snap) => snap,
            QueuePollParse::Errored(reason) => {
                panic!("expected valid parse, got errored: {reason}")
            }
        }
    }

    #[test]
    fn present_pr_is_enqueued() {
        let body = body_with_entries(&[7, 42, 9], false);
        let parse = parse_queue_snapshot(&body, 42);
        assert!(expect_valid(&parse).pr_found);
        assert_eq!(expect_valid(&parse).position, Some(1));
        assert_eq!(
            classify_poll(&parse, &ctx(true)),
            QueuePollClass::Enqueued { position: Some(1) }
        );
    }

    #[test]
    fn absent_and_never_seen_is_pr_not_found() {
        let body = body_with_entries(&[7, 9], false);
        let parse = parse_queue_snapshot(&body, 42);
        assert!(!expect_valid(&parse).pr_found);
        assert_eq!(
            classify_poll(&parse, &ctx(false)),
            QueuePollClass::PrNotFound
        );
    }

    #[test]
    fn absent_after_seen_in_full_queue_is_evicted() {
        let body = body_with_entries(&[7, 9], false);
        let parse = parse_queue_snapshot(&body, 42);
        assert!(!expect_valid(&parse).pr_found);
        assert_eq!(classify_poll(&parse, &ctx(true)), QueuePollClass::Evicted);
    }

    // --- F1: an errored / null poll body must never strand the PR ---

    #[test]
    fn errored_body_retries_in_place_not_terminal() {
        // A rate-limited / transient GraphQL body: null data + errors present.
        let body = serde_json::json!({
            "data": null,
            "errors": [ { "message": "API rate limit exceeded" } ]
        });
        let parse = parse_queue_snapshot(&body, 42);
        assert_eq!(
            parse,
            QueuePollParse::Errored("API rate limit exceeded".to_owned())
        );
        let class = classify_poll(&parse, &ctx(false));
        // The regression: a single transient blip must NOT be terminal PrNotFound.
        assert!(
            !class.is_terminal(),
            "errored body became terminal: {class:?}"
        );
        assert_eq!(
            class,
            QueuePollClass::PollError {
                reason: "API rate limit exceeded".to_owned()
            }
        );
    }

    #[test]
    fn errored_body_degrades_to_timed_out_never_pr_not_found() {
        let body = serde_json::json!({ "data": { "repository": null } });
        let parse = parse_queue_snapshot(&body, 42);
        assert!(matches!(parse, QueuePollParse::Errored(_)));
        let mut spent = ctx(false);
        spent.consecutive_errors = DEFAULT_ERROR_BUDGET;
        let class = classify_poll(&parse, &spent);
        assert_eq!(class, QueuePollClass::TimedOut);
        assert_ne!(class, QueuePollClass::PrNotFound);
        assert!(!class.is_terminal());
    }

    #[test]
    fn genuine_absence_in_valid_response_is_still_pr_not_found() {
        // The complementary guard: a VALID response with the PR absent and never
        // seen is still a legitimate terminal PrNotFound — F1 must not blunt it.
        let body = body_with_entries(&[7, 9], false);
        let parse = parse_queue_snapshot(&body, 42);
        assert_eq!(
            classify_poll(&parse, &ctx(false)),
            QueuePollClass::PrNotFound
        );
    }

    #[test]
    fn parses_exact_head_merge_state_and_latest_removal_reason() {
        let body = serde_json::json!({
            "data": { "repository": {
                "pullRequest": {
                    "headRefOid": "0123456789abcdef",
                    "merged": false,
                    "timelineItems": { "nodes": [
                        { "reason": "FAILED_CHECKS" }
                    ] }
                }
            } }
        });
        assert_eq!(
            parse_pr_observation(&body).expect("observation"),
            QueuePrObservation {
                head_sha: "0123456789abcdef".to_owned(),
                merged: false,
                removal_reason: Some("FAILED_CHECKS".to_owned()),
                removal_at: None,
            }
        );
    }

    #[test]
    fn malformed_pr_observation_fails_closed() {
        let body = serde_json::json!({
            "data": { "repository": { "pullRequest": {
                "merged": false,
                "timelineItems": { "nodes": [] }
            } } }
        });
        assert!(parse_pr_observation(&body).is_err());
    }

    #[test]
    fn evaluated_rules_detect_merge_queue_and_reject_malformed_body() {
        let rules = serde_json::json!([
            { "type": "required_status_checks" },
            { "type": "merge_queue", "parameters": {} }
        ]);
        assert_eq!(rules_require_merge_queue(&rules), Ok(true));
        assert_eq!(
            rules_require_merge_queue(&serde_json::json!([
                { "type": "required_status_checks" }
            ])),
            Ok(false)
        );
        assert!(rules_require_merge_queue(&serde_json::json!({"rules": []})).is_err());
    }

    // --- F2: a truncated page must not manufacture an eviction ---

    #[test]
    fn absent_on_truncated_page_is_not_evicted() {
        // PR 42 is absent from THIS page, but the queue reports more entries
        // beyond it (hasNextPage). It may live at position 101+. A seen PR that
        // has merely paged out must NOT be classified Evicted.
        let body = body_with_entries(&[7, 9, 11], true);
        let parse = parse_queue_snapshot(&body, 42);
        assert!(expect_valid(&parse).page_truncated);
        let class = classify_poll(&parse, &ctx(true));
        assert_ne!(
            class,
            QueuePollClass::Evicted,
            "truncated page misread as eviction (F2): {class:?}"
        );
    }

    fn positioned_page(nodes: &[(u64, u64)], has_next_page: bool) -> serde_json::Value {
        let nodes = nodes
            .iter()
            .map(|(pr, position)| {
                serde_json::json!({
                    "pullRequest": { "number": pr },
                    "position": position,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "data": { "repository": { "mergeQueue": { "entries": {
                "nodes": nodes,
                "pageInfo": { "hasNextPage": has_next_page },
            } } } }
        })
    }

    #[test]
    fn pr_on_second_page_is_enqueued_not_evicted() {
        // Full queue spans two pages; PR 42 lives at position 100 on page two.
        let page1 = positioned_page(&[(7, 0), (9, 1)], true);
        let page2 = positioned_page(&[(11, 99), (42, 100)], false);
        let parse = parse_queue_pages(&[page1, page2], 42);
        let snap = expect_valid(&parse);
        assert!(snap.pr_found);
        assert!(!snap.page_truncated);
        assert_eq!(
            classify_poll(&parse, &ctx(true)),
            QueuePollClass::Enqueued {
                position: Some(100)
            }
        );
    }

    #[test]
    fn proven_full_queue_absence_after_seen_is_still_evicted() {
        // Absent across the WHOLE drained queue (last page hasNextPage=false),
        // and previously seen -> a real eviction. The F2 guard must not suppress
        // this.
        let page1 = positioned_page(&[(7, 0), (9, 1)], true);
        let page2 = positioned_page(&[(11, 2), (13, 3)], false);
        let parse = parse_queue_pages(&[page1, page2], 42);
        let snap = expect_valid(&parse);
        assert!(!snap.pr_found);
        assert!(!snap.page_truncated);
        assert_eq!(classify_poll(&parse, &ctx(true)), QueuePollClass::Evicted);
    }

    #[test]
    fn errored_page_taints_pagination() {
        let page1 = positioned_page(&[(7, 0)], true);
        let page2 = serde_json::json!({
            "data": null,
            "errors": [ { "message": "API rate limit exceeded" } ]
        });
        let parse = parse_queue_pages(&[page1, page2], 42);
        assert!(matches!(parse, QueuePollParse::Errored(_)));
    }

    #[test]
    fn missing_nodes_is_errored_not_valid_empty_queue() {
        let body = serde_json::json!({
            "data": { "repository": { "mergeQueue": { "entries": {
                "pageInfo": { "hasNextPage": false }
            } } } }
        });
        assert!(matches!(
            parse_queue_snapshot(&body, 42),
            QueuePollParse::Errored(_)
        ));
    }

    #[test]
    fn missing_page_info_is_errored_not_proven_full_queue() {
        let body = serde_json::json!({
            "data": { "repository": { "mergeQueue": { "entries": {
                "nodes": []
            } } } }
        });
        assert!(matches!(
            parse_queue_snapshot(&body, 42),
            QueuePollParse::Errored(_)
        ));
    }

    // --- F3: a poll inside the settle window must not read lag as eviction ---

    #[test]
    fn absent_before_settle_window_is_not_evicted() {
        // First poll right after (re-)enqueue: the entries connection has not
        // caught up, so PR 42 momentarily reads absent from a full, valid queue.
        let body = body_with_entries(&[7, 9], false);
        let parse = parse_queue_snapshot(&body, 42);
        let mut c = ctx(true);
        c.attempt_elapsed = Duration::from_secs(2);
        c.settle_window = Duration::from_secs(10);
        let class = classify_poll(&parse, &c);
        assert_ne!(
            class,
            QueuePollClass::Evicted,
            "arm-write lag inside settle window misread as eviction (F3): {class:?}"
        );
    }

    #[test]
    fn never_seen_absence_before_settle_window_is_not_not_found() {
        let body = body_with_entries(&[7, 9], false);
        let parse = parse_queue_snapshot(&body, 42);
        let mut c = ctx(false);
        c.attempt_elapsed = Duration::from_secs(2);
        c.settle_window = Duration::from_secs(10);
        assert_eq!(
            classify_poll(&parse, &c),
            QueuePollClass::Enqueued { position: None }
        );
    }

    #[test]
    fn absent_after_settle_window_is_evicted() {
        // Same absence, but past the settle window -> a trustworthy eviction.
        let body = body_with_entries(&[7, 9], false);
        let parse = parse_queue_snapshot(&body, 42);
        let mut c = ctx(true);
        c.attempt_elapsed = Duration::from_secs(11);
        c.settle_window = Duration::from_secs(10);
        assert_eq!(classify_poll(&parse, &c), QueuePollClass::Evicted);
    }
}
