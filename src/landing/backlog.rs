//! The shape of the open-pull-request backlog.
//!
//! One number — "37 open" — says nothing about what is blocking them, and the
//! two plausible blockers call for opposite responses. A backlog that is
//! mostly `DIRTY` needs somebody to resolve conflicts, one pull request at a
//! time, and no amount of queue capacity helps. A backlog that is mostly
//! `BEHIND` or `BLOCKED` is waiting on gates, and hand-updating those is the
//! treadmill: with up-to-date protection on, every landing puts the rest back
//! to `BEHIND`.
//!
//! Counting by `mergeStateStatus` separates the two at a glance.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::fleet_service::Boundary;

/// Counts of open pull requests by mergeability, plus what could not be read.
#[derive(Clone, Debug, Serialize)]
pub struct BacklogFinding {
    /// Open pull requests targeting the base branch, when readable.
    pub total: Option<u64>,
    /// Lower-cased `mergeStateStatus` to count.
    pub by_state: BTreeMap<String, u64>,
    /// How many already have auto-merge enabled, which is what enqueues them.
    pub auto_merge_enabled: Option<u64>,
    /// How many are drafts, which cannot be enqueued at all.
    pub drafts: Option<u64>,
    /// Whether the pull-request list was truncated by paging.
    pub truncated: bool,
    /// Why the backlog could not be read.
    pub unreadable: Option<BacklogUnreadable>,
    /// Plain-language reading of the shape.
    pub interpretation: String,
}

/// Why the backlog read failed.
#[derive(Clone, Debug, Serialize)]
pub struct BacklogUnreadable {
    /// Which class of limit stopped the read.
    pub boundary: Boundary,
    /// What came back instead.
    pub detail: String,
}

/// An unreadable backlog, stated as such rather than as an empty one.
#[must_use]
pub fn unreadable(boundary: Boundary, detail: String) -> BacklogFinding {
    BacklogFinding {
        total: None,
        by_state: BTreeMap::new(),
        auto_merge_enabled: None,
        drafts: None,
        truncated: false,
        unreadable: Some(BacklogUnreadable { boundary, detail }),
        interpretation: "The open pull requests could not be read, so the backlog's shape is \
                         unknown. An empty count here would be indistinguishable from an empty \
                         backlog."
            .to_owned(),
    }
}

/// Count a GraphQL `repository.pullRequests` connection.
#[must_use]
pub fn from_graphql(connection: &Value) -> BacklogFinding {
    let nodes = connection
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut by_state: BTreeMap<String, u64> = BTreeMap::new();
    let mut auto_merge = 0u64;
    let mut drafts = 0u64;
    for node in &nodes {
        let state = node
            .get("mergeStateStatus")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_ascii_lowercase();
        *by_state.entry(state).or_insert(0) += 1;
        if node
            .get("autoMergeRequest")
            .is_some_and(|value| !value.is_null())
        {
            auto_merge += 1;
        }
        if node.get("isDraft").and_then(Value::as_bool) == Some(true) {
            drafts += 1;
        }
    }
    let total = connection
        .get("totalCount")
        .and_then(Value::as_u64)
        .unwrap_or(u64::try_from(nodes.len()).unwrap_or(u64::MAX));
    let truncated = connection
        .pointer("/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        == Some(true);
    let interpretation = interpret(&by_state, total, truncated);
    BacklogFinding {
        total: Some(total),
        by_state,
        auto_merge_enabled: Some(auto_merge),
        drafts: Some(drafts),
        truncated,
        unreadable: None,
        interpretation,
    }
}

fn interpret(by_state: &BTreeMap<String, u64>, total: u64, truncated: bool) -> String {
    if total == 0 {
        return "No open pull requests target this branch.".to_owned();
    }
    let dirty = by_state.get("dirty").copied().unwrap_or(0);
    let behind = by_state.get("behind").copied().unwrap_or(0);
    let blocked = by_state.get("blocked").copied().unwrap_or(0);
    let clean = by_state.get("clean").copied().unwrap_or(0);
    let unstable = by_state.get("unstable").copied().unwrap_or(0);

    let mut parts = Vec::new();
    if dirty > 0 {
        parts.push(format!(
            "{dirty} have merge conflicts and need a human or an agent to resolve them; no \
             amount of queue capacity moves these"
        ));
    }
    if behind > 0 {
        parts.push(format!(
            "{behind} are merely out of date with the base - a queue absorbs these, and updating \
             them by hand is the treadmill"
        ));
    }
    if blocked > 0 {
        parts.push(format!(
            "{blocked} are waiting on a required check or review, which is a capacity or gate \
             question, not a conflict one"
        ));
    }
    if unstable > 0 {
        parts.push(format!(
            "{unstable} have a failing non-required check and can still merge"
        ));
    }
    if clean > 0 {
        parts.push(format!("{clean} are ready to land right now"));
    }
    if parts.is_empty() {
        parts.push("no pull request carries a recognized mergeability state".to_owned());
    }
    let mut text = format!("{total} open: {}.", parts.join("; "));
    if truncated {
        text.push_str(
            " The list was truncated by paging, so these counts are a lower bound rather than the \
             whole backlog.",
        );
    }
    text
}
