//! Classify one pull request's merge-queue state from GitHub's GraphQL facts.
//!
//! ## Why this is not read from `auto_merge`
//!
//! GitHub **consumes** a pull request's auto-merge request when the merge
//! queue admits it, and emits no `AutoMergeDisabledEvent` when it does. So
//! REST `pulls/<n>.auto_merge` and GraphQL `autoMergeRequest` are `null` for
//! every queued pull request. A reader that treats that `null` as "unarmed"
//! re-arms a queued pull request, and under `ALLGREEN` grouping a re-enqueue
//! of a head that already failed its checks fails every batch-mate with it.
//! Queue membership is `isInMergeQueue` / `mergeQueueEntry`, nothing else.
//!
//! ## How "a new head since the ejection" is decided
//!
//! By SHA, never by date and never by timeline position alone. GitHub orders
//! `PullRequestCommit` timeline items by the commit's own (author-controlled)
//! date, not by when it was pushed: a fix committed before an ejection and
//! pushed after it sits *before* the removal in the timeline. So the removed
//! head is read from the removal itself and compared with `headRefOid`.
//!
//! `RemovedFromMergeQueueEvent.beforeCommit` is the merge-group commit the
//! queue built, not the pull request's head: its second parent is the head the
//! queue removed. That parent is trusted only when it is a commit this pull
//! request is seen to have had (a `PullRequestCommit`, a force-push
//! `afterCommit`, or `headRefOid`). A removal GitHub never built a merge group
//! for (`merge_conflict`) has no `beforeCommit`; there, and whenever the
//! removed head cannot be established, the classifier falls back to push-time
//! evidence in timeline order: a `HeadRefForcePushedEvent` or
//! `PullRequestCommit` *after* the removal whose oid is the current head. With
//! neither, the answer is "no new head", which refuses a re-arm (fail closed).
//!
//! ## Limits
//!
//! The query reads `timelineItems(last: 100)`. That is a window: when
//! `pageInfo.hasPreviousPage` is `true`, [`PrQueueReport::requeues_without_new_head`]
//! is a lower bound and [`PrQueueReport::timeline_complete`] says so. A
//! capture that did not request `pageInfo` reports `None` (unmeasured), never
//! `Some(true)`.
//!
//! Actor identity is deliberately not consulted: every queue mutation in the
//! observed repositories is attributed to the same App actor whether Shipyard
//! or an agent driving `ghapp` issued it, so it cannot separate the two.
//!
//! The real-response corpus that pins this behaviour lives in
//! `tests/fixtures/github/`; `scripts/ghapp_queue_arm_guard.py` carries a
//! Python twin of this classifier asserted against the same expectations.

use serde::Serialize;
use serde_json::Value;

/// GraphQL document whose response [`classify_pr_queue_state`] consumes.
///
/// Variables: `owner`, `name`, `number`.
pub const PR_QUEUE_STATE_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){number state headRefOid isInMergeQueue mergeQueueEntry{state position} autoMergeRequest{enabledAt} timelineItems(last:100,itemTypes:[PULL_REQUEST_COMMIT,HEAD_REF_FORCE_PUSHED_EVENT,ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT,AUTO_MERGE_ENABLED_EVENT,AUTO_MERGE_DISABLED_EVENT,MERGED_EVENT]){pageInfo{hasPreviousPage} nodes{__typename ... on PullRequestCommit{commit{oid}} ... on HeadRefForcePushedEvent{createdAt afterCommit{oid}} ... on AddedToMergeQueueEvent{createdAt actor{login}} ... on RemovedFromMergeQueueEvent{createdAt reason actor{login} beforeCommit{oid parents(first:3){nodes{oid}}}} ... on AutoMergeEnabledEvent{createdAt actor{login}} ... on AutoMergeDisabledEvent{createdAt reason actor{login}} ... on MergedEvent{createdAt}}}}}}";

/// Fixed preface printed ahead of every classification a human or agent reads.
pub const REST_AUTO_MERGE_PREFACE: &str = "REST pulls/<n>.auto_merge is null for every queued PR \
     (GitHub consumes auto-merge on enqueue) — never read it as 'unarmed'.";

/// Removal reasons after which re-adding the *same* head is a known failure:
/// the head's checks already failed, or it already conflicted. Under `ALLGREEN`
/// grouping such a re-add fails every batch-mate with it.
const RETRY_HAZARD_REASONS: &[&str] = &["failed_checks", "merge_conflict"];

/// Whether re-enqueuing the unchanged head after a removal for `reason` is the
/// `ALLGREEN` cascade (`failed_checks`, `merge_conflict`).
#[must_use]
pub fn same_head_requeue_cascades(reason: &str) -> bool {
    RETRY_HAZARD_REASONS
        .iter()
        .any(|hazard| reason.eq_ignore_ascii_case(hazard))
}

/// Whether the unchanged head may be re-enqueued after a removal for `reason`.
///
/// Only `invalid_merge_commit`: GitHub failed to build the merge commit, which
/// says nothing against the head, and it is the one removal Shipyard's own
/// admission policy re-arms after. Every other reason needs either a new head
/// or a person who knows why it was removed.
#[must_use]
pub fn same_head_requeue_allowed(reason: &str) -> bool {
    reason.eq_ignore_ascii_case("invalid_merge_commit")
}

/// The merge-queue state of one pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum PrQueueState {
    /// `state == MERGED`.
    Merged,
    /// `state == CLOSED`.
    Closed,
    /// In the merge queue now.
    Queued {
        /// `mergeQueueEntry.state`.
        entry_state: Option<String>,
        /// `mergeQueueEntry.position`.
        position: Option<u64>,
        /// Re-adds of an unchanged head after a failing removal, over the
        /// visible timeline.
        requeues_without_new_head: u32,
    },
    /// Auto-merge is armed and the queue has not admitted the PR yet.
    ArmedNotQueued {
        /// `autoMergeRequest.enabledAt`.
        enabled_at: Option<String>,
        /// Re-adds of an unchanged head after a failing removal.
        requeues_without_new_head: u32,
    },
    /// Open, not armed, and no queue removal is visible in the timeline
    /// window.
    NeverArmed,
    /// The last queue removal was not a merge and the PR is neither queued nor
    /// armed.
    Ejected {
        /// `RemovedFromMergeQueueEvent.reason` of the last removal.
        reason: String,
        /// `RemovedFromMergeQueueEvent.createdAt` of the last removal.
        at: Option<String>,
        /// Whether a commit or force-push follows the last removal.
        new_head_since_removal: bool,
        /// Re-adds of an unchanged head after a failing removal.
        requeues_without_new_head: u32,
    },
    /// The response could not be classified. Never treated as absence.
    Unknown {
        /// What was missing or malformed.
        detail: String,
    },
}

/// One fact the classification rests on, with the field it was read from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueueFact {
    /// Short fact name.
    pub name: String,
    /// The value read.
    pub value: Value,
    /// Field path inside the GraphQL response.
    pub source: String,
}

/// The last non-merge removal from the queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Ejection {
    /// Removal reason, as GitHub spells it.
    pub reason: String,
    /// When the removal happened.
    pub at: Option<String>,
    /// Whether the current head differs from the head the queue removed.
    pub new_head_since: bool,
    /// The head the queue removed, when the removal names it.
    pub removed_head: Option<String>,
    /// How [`Ejection::new_head_since`] was decided.
    pub new_head_basis: NewHeadBasis,
    /// Timeline index of the removal item.
    pub timeline_index: usize,
    /// `beforeCommit.oid`: the merge-group commit the queue built and ran the
    /// required checks on, when the removal names one (`merge_conflict`
    /// removals do not).
    pub merge_group_commit: Option<String>,
}

/// How "a new head since the ejection" was decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NewHeadBasis {
    /// `headRefOid` compared with the head the removal names.
    RemovedHeadSha,
    /// The removal names no head; a force-push or commit after it in timeline
    /// order whose oid is `headRefOid`.
    PushAfterRemoval,
    /// Neither: no evidence of a new head, so none is assumed.
    NoEvidence,
}

/// A classification plus every fact that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PrQueueReport {
    /// PR number, when present in the response.
    pub pr: Option<u64>,
    /// `headRefOid`.
    pub head_oid: Option<String>,
    /// The classification.
    pub state: PrQueueState,
    /// The last queue removal, when it was not a merge.
    pub last_ejection: Option<Ejection>,
    /// Re-adds of an unchanged head after a failing removal.
    pub requeues_without_new_head: u32,
    /// `failed_checks` / `merge_conflict` removals of the *current* head over
    /// the visible timeline: the removals a same-head re-enqueue would repeat.
    /// A `manual` or `invalid_merge_commit` removal says nothing against the
    /// head and is not counted. A removal that names its head counts when that head is
    /// `headRefOid`; one that names none counts when it follows the first
    /// timeline item that pushed `headRefOid` (or when no such item is
    /// visible), so an unattributable removal is charged to the current head
    /// rather than assumed away.
    pub ejections_of_current_head: u32,
    /// `Some(true)` when the timeline window reached the start of history,
    /// `Some(false)` when older items were cut off, `None` when unmeasured.
    pub timeline_complete: Option<bool>,
    /// Every fact consulted, with its source field.
    pub facts: Vec<QueueFact>,
}

/// Classify the pull request in a GraphQL response.
#[must_use]
pub fn classify_pr_queue_state(response: &Value) -> PrQueueState {
    explain_pr_queue_state(response).state
}

fn pull_request_node(response: &Value) -> Option<(&Value, &'static str)> {
    if let Some(node) = response
        .pointer("/data/repository/pullRequest")
        .filter(|node| node.is_object())
    {
        return Some((node, "data.repository.pullRequest"));
    }
    if let Some(node) = response
        .pointer("/data/node")
        .filter(|node| node.is_object())
    {
        return Some((node, "data.node"));
    }
    (response.is_object() && response.get("state").is_some()).then_some((response, "pullRequest"))
}

fn unknown(detail: impl Into<String>, facts: Vec<QueueFact>) -> PrQueueReport {
    PrQueueReport {
        pr: None,
        head_oid: None,
        state: PrQueueState::Unknown {
            detail: detail.into(),
        },
        last_ejection: None,
        requeues_without_new_head: 0,
        ejections_of_current_head: 0,
        timeline_complete: None,
        facts,
    }
}

fn is_new_head(typename: &str) -> bool {
    matches!(typename, "PullRequestCommit" | "HeadRefForcePushedEvent")
}

/// The commit a new-head timeline item put on the branch.
fn pushed_oid(node: &Value) -> Option<&str> {
    match node.get("__typename").and_then(Value::as_str)? {
        "PullRequestCommit" => node.pointer("/commit/oid").and_then(Value::as_str),
        "HeadRefForcePushedEvent" => node.pointer("/afterCommit/oid").and_then(Value::as_str),
        _ => None,
    }
}

/// The head a `RemovedFromMergeQueueEvent` removed: the second parent of its
/// `beforeCommit` merge-group commit, accepted only when it is one of `known`
/// (commits this pull request is seen to have had). `beforeCommit` itself is
/// accepted when it is already one of them.
fn removed_head(node: &Value, known: &[&str]) -> Option<String> {
    let before = node.get("beforeCommit").filter(|value| value.is_object())?;
    let is_known = |oid: &str| known.iter().any(|seen| seen.eq_ignore_ascii_case(oid));
    if let Some(oid) = before.get("oid").and_then(Value::as_str)
        && is_known(oid)
    {
        return Some(oid.to_owned());
    }
    let parents = before
        .pointer("/parents/nodes")
        .and_then(Value::as_array)?
        .iter()
        .map(|parent| parent.get("oid").and_then(Value::as_str))
        .collect::<Option<Vec<_>>>()?;
    match parents.as_slice() {
        [_, head] if is_known(head) => Some((*head).to_owned()),
        _ => None,
    }
}

/// Whether the first re-add (at `add`) after the hazard removal at `removal`
/// re-added the head that removal removed, decided by SHA: against the next
/// removal's head, or against `queued_head` (`headRefOid` while the pull
/// request is still queued) when no removal follows. `None` when the SHAs
/// cannot say, and the caller falls back to timeline order.
fn same_head_re_add(
    nodes: &[Value],
    removed_heads: &[Option<String>],
    removal: usize,
    add: usize,
    queued_head: Option<&str>,
) -> Option<bool> {
    let removed = removed_heads.get(removal)?.as_deref()?;
    let next_removal = nodes.iter().enumerate().skip(add + 1).find(|(_, node)| {
        node.get("__typename").and_then(Value::as_str) == Some("RemovedFromMergeQueueEvent")
    });
    let re_added = match next_removal {
        Some((index, _)) => removed_heads.get(index)?.as_deref()?,
        None => queued_head?,
    };
    Some(re_added.eq_ignore_ascii_case(removed))
}

/// Whether `head` is a new head since the removal at `index`, and on what basis.
fn new_head_since(
    nodes: &[Value],
    index: usize,
    removed: Option<&str>,
    head: Option<&str>,
) -> (bool, NewHeadBasis) {
    let Some(head) = head else {
        return (false, NewHeadBasis::NoEvidence);
    };
    if let Some(removed) = removed {
        return (
            !removed.eq_ignore_ascii_case(head),
            NewHeadBasis::RemovedHeadSha,
        );
    }
    if nodes
        .iter()
        .skip(index + 1)
        .filter_map(pushed_oid)
        .any(|oid| oid.eq_ignore_ascii_case(head))
    {
        (true, NewHeadBasis::PushAfterRemoval)
    } else {
        (false, NewHeadBasis::NoEvidence)
    }
}

/// Classify and report every fact, with the field path it came from.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn explain_pr_queue_state(response: &Value) -> PrQueueReport {
    let mut facts = Vec::new();
    if let Some(errors) = response.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let messages = errors
            .iter()
            .filter_map(|error| error.get("message").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("; ");
        return unknown(format!("GraphQL errors: {messages}"), facts);
    }
    let Some((pr, root)) = pull_request_node(response) else {
        return unknown("response carries no pull request object", facts);
    };
    let fact = |facts: &mut Vec<QueueFact>, name: &str, value: Value, field: &str| {
        facts.push(QueueFact {
            name: name.to_owned(),
            value,
            source: format!("{root}.{field}"),
        });
    };

    let number = pr.get("number").and_then(Value::as_u64);
    let head_oid = pr
        .get("headRefOid")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let Some(state) = pr.get("state").and_then(Value::as_str) else {
        return unknown(format!("{root}.state is missing"), facts);
    };
    fact(&mut facts, "state", Value::from(state), "state");
    let Some(in_queue) = pr.get("isInMergeQueue").and_then(Value::as_bool) else {
        return unknown(format!("{root}.isInMergeQueue is missing"), facts);
    };
    fact(
        &mut facts,
        "is_in_merge_queue",
        Value::from(in_queue),
        "isInMergeQueue",
    );
    let entry = pr.get("mergeQueueEntry").filter(|entry| !entry.is_null());
    fact(
        &mut facts,
        "merge_queue_entry",
        entry.cloned().unwrap_or(Value::Null),
        "mergeQueueEntry",
    );
    let auto_merge = pr.get("autoMergeRequest").filter(|value| !value.is_null());
    fact(
        &mut facts,
        "auto_merge_request",
        auto_merge.cloned().unwrap_or(Value::Null),
        "autoMergeRequest",
    );
    let Some(nodes) = pr.pointer("/timelineItems/nodes").and_then(Value::as_array) else {
        return unknown(format!("{root}.timelineItems.nodes is missing"), facts);
    };
    let timeline_complete = pr
        .pointer("/timelineItems/pageInfo/hasPreviousPage")
        .and_then(Value::as_bool)
        .map(|has_previous| !has_previous);
    fact(
        &mut facts,
        "timeline_complete",
        timeline_complete.map_or(Value::Null, Value::from),
        "timelineItems.pageInfo.hasPreviousPage",
    );

    // Every commit this pull request is seen to have had: a removed head read
    // off a merge-group commit is trusted only when it is one of these.
    let known_heads = nodes
        .iter()
        .filter_map(pushed_oid)
        .chain(head_oid.as_deref())
        .collect::<Vec<_>>();
    let removed_heads = nodes
        .iter()
        .map(|node| {
            (node.get("__typename").and_then(Value::as_str) == Some("RemovedFromMergeQueueEvent"))
                .then(|| removed_head(node, &known_heads))
                .flatten()
        })
        .collect::<Vec<_>>();

    // Walk the timeline once, in its own order.
    let mut requeues = 0_u32;
    let mut requeue_indices = Vec::new();
    // The hazard removal awaiting its first re-add, and whether a push has
    // followed it in timeline order (the fallback when SHAs cannot decide).
    let mut hazard_pending: Option<(usize, bool)> = None;
    let mut last_removal: Option<(usize, String, Option<String>)> = None;
    let mut last_new_head: Option<usize> = None;
    for (index, node) in nodes.iter().enumerate() {
        let typename = node.get("__typename").and_then(Value::as_str).unwrap_or("");
        if is_new_head(typename) {
            if let Some((_, pushed)) = hazard_pending.as_mut() {
                *pushed = true;
            }
            last_new_head = Some(index);
            continue;
        }
        match typename {
            "RemovedFromMergeQueueEvent" => {
                let reason = node
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("UNKNOWN")
                    .to_owned();
                hazard_pending = same_head_requeue_cascades(&reason).then_some((index, false));
                let at = node
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                last_removal = Some((index, reason, at));
            }
            "AddedToMergeQueueEvent" => {
                if let Some((removal, pushed)) = hazard_pending
                    && same_head_re_add(
                        nodes,
                        &removed_heads,
                        removal,
                        index,
                        in_queue.then_some(head_oid.as_deref()).flatten(),
                    )
                    .unwrap_or(!pushed)
                {
                    requeues += 1;
                    requeue_indices.push(index);
                }
                hazard_pending = None;
            }
            _ => {}
        }
    }
    fact(
        &mut facts,
        "requeues_without_new_head",
        Value::from(requeues),
        &format!(
            "timelineItems.nodes{requeue_indices:?} (AddedToMergeQueueEvent directly after a \
             failed_checks/merge_conflict RemovedFromMergeQueueEvent of the same head: the \
             removed heads' SHAs when both removals name one, else no commit or force-push \
             between in timeline order)"
        ),
    );

    let last_removal_seen = last_removal.as_ref().map(|(index, _, _)| *index);
    let last_ejection = last_removal
        .filter(|(_, reason, _)| !reason.eq_ignore_ascii_case("merged"))
        .map(|(index, reason, at)| {
            let removed_head = removed_heads[index].clone();
            let (new_head_since, new_head_basis) =
                new_head_since(nodes, index, removed_head.as_deref(), head_oid.as_deref());
            Ejection {
                new_head_since,
                removed_head,
                new_head_basis,
                reason,
                at,
                timeline_index: index,
                merge_group_commit: nodes[index]
                    .pointer("/beforeCommit/oid")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            }
        });
    let ejections_of_current_head =
        count_ejections_of_head(nodes, &removed_heads, head_oid.as_deref());
    fact(
        &mut facts,
        "ejections_of_current_head",
        Value::from(ejections_of_current_head),
        "timelineItems.nodes[] (failed_checks/merge_conflict RemovedFromMergeQueueEvent whose \
         removed head is headRefOid; a removal naming no head counts once it follows the first \
         push of headRefOid)",
    );
    if let Some(ejection) = &last_ejection {
        fact(
            &mut facts,
            "last_ejection",
            serde_json::json!({
                "reason": ejection.reason,
                "at": ejection.at,
                "new_head_since": ejection.new_head_since,
                "removed_head": ejection.removed_head,
                "head": head_oid,
                "basis": ejection.new_head_basis,
            }),
            &format!(
                "timelineItems.nodes[{}] (RemovedFromMergeQueueEvent; {})",
                ejection.timeline_index,
                match ejection.new_head_basis {
                    NewHeadBasis::RemovedHeadSha =>
                        "new head = headRefOid differs from beforeCommit's second parent",
                    NewHeadBasis::PushAfterRemoval =>
                        "no removed head named; new head = a later PullRequestCommit or \
                         HeadRefForcePushedEvent whose oid is headRefOid",
                    NewHeadBasis::NoEvidence =>
                        "no removed head named and no later push of headRefOid; no new head \
                         assumed",
                }
            ),
        );
    }

    let state = match state.to_ascii_uppercase().as_str() {
        "MERGED" => PrQueueState::Merged,
        "CLOSED" => PrQueueState::Closed,
        "OPEN" if in_queue => PrQueueState::Queued {
            entry_state: entry
                .and_then(|entry| entry.get("state"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            position: entry
                .and_then(|entry| entry.get("position"))
                .and_then(Value::as_u64),
            requeues_without_new_head: requeues,
        },
        "OPEN" if auto_merge.is_some() => PrQueueState::ArmedNotQueued {
            enabled_at: auto_merge
                .and_then(|request| request.get("enabledAt"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            requeues_without_new_head: requeues,
        },
        "OPEN" => match &last_ejection {
            Some(ejection) => PrQueueState::Ejected {
                reason: ejection.reason.clone(),
                at: ejection.at.clone(),
                new_head_since_removal: ejection.new_head_since,
                requeues_without_new_head: requeues,
            },
            // A truncated window with no removal and no new head in it could
            // be hiding an older same-head ejection; that is not "never armed".
            None if timeline_complete == Some(false)
                && last_removal_seen.is_none()
                && last_new_head.is_none() =>
            {
                PrQueueState::Unknown {
                    detail: "timelineItems window is truncated and contains no queue removal and \
                             no new head, so an older ejection of this head cannot be ruled out"
                        .to_owned(),
                }
            }
            None => PrQueueState::NeverArmed,
        },
        other => {
            return PrQueueReport {
                pr: number,
                head_oid,
                state: PrQueueState::Unknown {
                    detail: format!("unrecognized pull request state `{other}`"),
                },
                last_ejection,
                requeues_without_new_head: requeues,
                ejections_of_current_head,
                timeline_complete,
                facts,
            };
        }
    };
    PrQueueReport {
        pr: number,
        head_oid,
        state,
        last_ejection,
        requeues_without_new_head: requeues,
        ejections_of_current_head,
        timeline_complete,
        facts,
    }
}

/// `failed_checks` / `merge_conflict` removals of `head`; see
/// [`PrQueueReport::ejections_of_current_head`].
fn count_ejections_of_head(
    nodes: &[Value],
    removed_heads: &[Option<String>],
    head: Option<&str>,
) -> u32 {
    let Some(head) = head else {
        return 0;
    };
    let first_push = nodes
        .iter()
        .position(|node| pushed_oid(node).is_some_and(|oid| oid.eq_ignore_ascii_case(head)));
    let mut count = 0_u32;
    for (index, node) in nodes.iter().enumerate() {
        if node.get("__typename").and_then(Value::as_str) != Some("RemovedFromMergeQueueEvent") {
            continue;
        }
        let reason = node.get("reason").and_then(Value::as_str).unwrap_or("");
        if !same_head_requeue_cascades(reason) {
            continue;
        }
        let charged = match removed_heads.get(index).and_then(Option::as_deref) {
            Some(removed) => removed.eq_ignore_ascii_case(head),
            None => first_push.is_none_or(|first| index > first),
        };
        if charged {
            count += 1;
        }
    }
    count
}

impl PrQueueState {
    /// Stable snake-case class name, identical to the serialized tag.
    #[must_use]
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Queued { .. } => "queued",
            Self::ArmedNotQueued { .. } => "armed_not_queued",
            Self::NeverArmed => "never_armed",
            Self::Ejected { .. } => "ejected",
            Self::Unknown { .. } => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/github");

    fn fixture(name: &str) -> Value {
        let raw = std::fs::read_to_string(format!("{FIXTURES}/{name}")).expect("fixture");
        serde_json::from_str(&raw).expect("fixture JSON")
    }

    fn expected() -> Value {
        fixture("expected_classifications.json")
    }

    #[test]
    fn queued_pr_with_null_auto_merge_is_queued_not_unarmed() {
        let report = explain_pr_queue_state(&fixture("pr_queued.json"));
        assert_eq!(report.pr, Some(8669));
        assert_eq!(
            report.state,
            PrQueueState::Queued {
                entry_state: Some("AWAITING_CHECKS".to_owned()),
                position: Some(1),
                requeues_without_new_head: 0,
            }
        );
        assert!(report.last_ejection.is_none());
        let auto_merge = report
            .facts
            .iter()
            .find(|fact| fact.name == "auto_merge_request")
            .expect("auto-merge fact");
        assert!(auto_merge.value.is_null());
        assert_eq!(
            auto_merge.source,
            "data.repository.pullRequest.autoMergeRequest"
        );
    }

    #[test]
    fn never_armed_pr_is_never_armed() {
        assert_eq!(
            classify_pr_queue_state(&fixture("pr_never_armed.json")),
            PrQueueState::NeverArmed
        );
    }

    #[test]
    fn armed_pr_outside_the_queue_is_armed_not_queued() {
        assert_eq!(
            classify_pr_queue_state(&fixture("pr_armed_not_queued.json")),
            PrQueueState::ArmedNotQueued {
                enabled_at: Some("2026-09-21T23:10:04Z".to_owned()),
                requeues_without_new_head: 0,
            }
        );
    }

    #[test]
    fn re_enqueue_without_new_head_is_counted_on_a_queued_pr() {
        let report = explain_pr_queue_state(&fixture("pr_ejected_requeued.json"));
        assert_eq!(
            report.state,
            PrQueueState::Queued {
                entry_state: Some("AWAITING_CHECKS".to_owned()),
                position: Some(3),
                requeues_without_new_head: 1,
            }
        );
        let ejection = report.last_ejection.expect("ejection history");
        assert_eq!(ejection.reason, "failed_checks");
        assert!(!ejection.new_head_since);
    }

    #[test]
    fn ejection_history_counts_same_head_requeues_but_not_the_fixed_conflict() {
        let report = explain_pr_queue_state(&fixture("pr_ejected_history.json"));
        // The merge_conflict removal is followed by a commit before its re-add,
        // so it is not counted; each of the four failed_checks re-adds is.
        assert_eq!(report.requeues_without_new_head, 4);
        assert_eq!(
            report.state,
            PrQueueState::ArmedNotQueued {
                enabled_at: Some("2026-09-22T18:57:52Z".to_owned()),
                requeues_without_new_head: 4,
            }
        );
        let ejection = report.last_ejection.expect("ejection history");
        assert_eq!(ejection.reason, "failed_checks");
        assert!(ejection.new_head_since);
    }

    #[test]
    fn merged_removal_is_not_an_ejection() {
        let report = explain_pr_queue_state(&fixture("pr_merged.json"));
        assert_eq!(report.state, PrQueueState::Merged);
        assert!(report.last_ejection.is_none());
        let mut open = fixture("pr_merged.json");
        open["data"]["repository"]["pullRequest"]["state"] = Value::from("OPEN");
        // Even with the state forced open, a `merged` removal never reads as
        // an ejection.
        assert_eq!(classify_pr_queue_state(&open), PrQueueState::NeverArmed);
    }

    #[test]
    fn every_fixture_matches_the_shared_expectations() {
        let expected = expected();
        let mut checked = 0;
        for (name, want) in expected.as_object().expect("object") {
            if name.starts_with('_') {
                continue;
            }
            let report = explain_pr_queue_state(&fixture(name));
            assert_eq!(report.state.class(), want["class"], "{name}");
            assert_eq!(report.pr, want["pr"].as_u64(), "{name}");
            assert_eq!(
                u64::from(report.requeues_without_new_head),
                want["requeues_without_new_head"].as_u64().expect("count"),
                "{name}"
            );
            assert_eq!(
                report
                    .last_ejection
                    .as_ref()
                    .map_or(Value::Null, |ejection| Value::from(ejection.reason.clone())),
                want["last_ejection_reason"],
                "{name}"
            );
            if let Some(new_head) = want.get("last_ejection_new_head_since") {
                assert_eq!(
                    report
                        .last_ejection
                        .as_ref()
                        .map(|ejection| ejection.new_head_since),
                    new_head.as_bool(),
                    "{name}"
                );
            }
            if let Some(count) = want.get("ejections_of_current_head") {
                assert_eq!(
                    Some(u64::from(report.ejections_of_current_head)),
                    count.as_u64(),
                    "{name}"
                );
            }
            if let Some(commit) = want.get("merge_group_commit") {
                assert_eq!(
                    report
                        .last_ejection
                        .as_ref()
                        .and_then(|ejection| ejection.merge_group_commit.as_deref()),
                    commit.as_str(),
                    "{name}"
                );
            }
            match &report.state {
                PrQueueState::Queued {
                    entry_state,
                    position,
                    ..
                } => {
                    assert_eq!(entry_state.as_deref(), want["entry_state"].as_str());
                    assert_eq!(*position, want["position"].as_u64());
                }
                PrQueueState::ArmedNotQueued { enabled_at, .. } => {
                    assert_eq!(enabled_at.as_deref(), want["enabled_at"].as_str());
                }
                PrQueueState::Ejected {
                    reason,
                    new_head_since_removal,
                    ..
                } => {
                    assert_eq!(Some(reason.as_str()), want["reason"].as_str(), "{name}");
                    assert_eq!(
                        Some(*new_head_since_removal),
                        want["new_head_since_removal"].as_bool(),
                        "{name}"
                    );
                }
                _ => {}
            }
            checked += 1;
        }
        // Control: the loop must actually have visited the whole corpus,
        // including the real ejected captures and the labelled synthetic ones.
        assert_eq!(checked, 15);
    }

    #[test]
    fn ejected_pr_reports_whether_a_new_head_followed() {
        // Derived from the real 8702 capture: drop the final re-add and mark
        // it out of the queue, which is the state an agent sees between the
        // ejection and its re-enqueue.
        let mut value = fixture("pr_ejected_requeued.json");
        let pr = &mut value["data"]["repository"]["pullRequest"];
        pr["isInMergeQueue"] = Value::from(false);
        pr["mergeQueueEntry"] = Value::Null;
        pr["timelineItems"]["nodes"]
            .as_array_mut()
            .expect("nodes")
            .pop();
        assert_eq!(
            classify_pr_queue_state(&value),
            PrQueueState::Ejected {
                reason: "failed_checks".to_owned(),
                at: Some("2026-09-22T20:29:00Z".to_owned()),
                new_head_since_removal: false,
                requeues_without_new_head: 0,
            }
        );
        // A force-push that names no commit is not evidence of a new head.
        value["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]
            .as_array_mut()
            .expect("nodes")
            .push(serde_json::json!({"__typename": "HeadRefForcePushedEvent"}));
        assert!(matches!(
            classify_pr_queue_state(&value),
            PrQueueState::Ejected {
                new_head_since_removal: false,
                ..
            }
        ));
        // One that put the current head on the branch is.
        value["data"]["repository"]["pullRequest"]["headRefOid"] = Value::from(PUSHED_HEAD);
        value["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]
            .as_array_mut()
            .expect("nodes")
            .push(serde_json::json!({
                "__typename": "HeadRefForcePushedEvent",
                "afterCommit": {"oid": PUSHED_HEAD},
            }));
        assert!(matches!(
            classify_pr_queue_state(&value),
            PrQueueState::Ejected {
                new_head_since_removal: true,
                ..
            }
        ));
    }

    #[test]
    fn unreadable_responses_are_unknown_never_never_armed() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"errors": [{"message": "rate limited"}]}),
            serde_json::json!({"data": {"repository": {"pullRequest": {"state": "OPEN"}}}}),
            serde_json::json!({"data": {"repository": {"pullRequest": {
                "state": "OPEN", "isInMergeQueue": false}}}}),
        ] {
            assert!(
                matches!(
                    classify_pr_queue_state(&value),
                    PrQueueState::Unknown { .. }
                ),
                "{value}"
            );
        }
    }

    #[test]
    fn real_truncated_same_head_ejection_is_ejected_without_new_head() {
        // 8638's real timeline cut right after a failed_checks removal that was
        // followed, live, by a same-head re-enqueue.
        let value = fixture("pr_real_truncated_same_head_ejected.json");
        assert_eq!(value["_provenance"]["source_pr"], "Generous-Corp/pulp#8638");
        assert_eq!(
            classify_pr_queue_state(&value),
            PrQueueState::Ejected {
                reason: "failed_checks".to_owned(),
                at: Some("2026-09-22T10:26:42Z".to_owned()),
                new_head_since_removal: false,
                requeues_without_new_head: 3,
            }
        );
    }

    #[test]
    fn truncated_window_without_removal_or_new_head_is_unknown() {
        let value = fixture("pr_synthetic_truncated_window.json");
        assert!(matches!(
            classify_pr_queue_state(&value),
            PrQueueState::Unknown { .. }
        ));
        // Control: the same window marked complete is an ordinary never-armed PR.
        let mut complete = value;
        complete["data"]["repository"]["pullRequest"]["timelineItems"]["pageInfo"]["hasPreviousPage"] =
            Value::from(false);
        assert_eq!(classify_pr_queue_state(&complete), PrQueueState::NeverArmed);
    }

    #[test]
    fn truncated_timeline_is_reported_not_hidden() {
        let mut value = fixture("pr_never_armed.json");
        value["data"]["repository"]["pullRequest"]["timelineItems"]["pageInfo"] =
            serde_json::json!({"hasPreviousPage": true});
        assert_eq!(
            explain_pr_queue_state(&value).timeline_complete,
            Some(false)
        );
        assert_eq!(
            explain_pr_queue_state(&fixture("pr_never_armed.json")).timeline_complete,
            None
        );
    }

    const PUSHED_HEAD: &str = "1111111111111111111111111111111111111111";
    const BACKDATED_FIXTURE: &str = "pr_real_8912_backdated_fix_after_ejection.json";
    /// The head the queue removed from 8912: `beforeCommit`'s second parent.
    const REMOVED_8912: &str = "f4b97cb8e4458072e2ff10429c2361ec3b446b5e";
    /// The back-dated fix pushed after the ejection.
    const FIX_8912: &str = "c246e05b54069e800a8d1e5f8f4c9d5e841818d7";

    fn pr_mut(value: &mut Value) -> &mut Value {
        &mut value["data"]["repository"]["pullRequest"]
    }

    fn nodes_mut(value: &mut Value) -> &mut Vec<Value> {
        pr_mut(value)["timelineItems"]["nodes"]
            .as_array_mut()
            .expect("nodes")
    }

    fn removal_index(value: &Value) -> usize {
        value["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .rposition(|node| node["__typename"] == "RemovedFromMergeQueueEvent")
            .expect("removal")
    }

    #[test]
    fn backdated_fix_pushed_after_the_ejection_is_a_new_head() {
        let value = fixture(BACKDATED_FIXTURE);
        assert_eq!(value["_provenance"]["source_pr"], "Generous-Corp/pulp#8912");
        // Control: GitHub really does sort the back-dated fix BEFORE the
        // removal, so timeline position alone would say "no new head".
        let nodes = value["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]
            .as_array()
            .expect("nodes");
        let fix_index = nodes
            .iter()
            .position(|node| node["commit"]["oid"] == FIX_8912)
            .expect("fix commit");
        assert!(fix_index < removal_index(&value));
        let report = explain_pr_queue_state(&value);
        let ejection = report.last_ejection.expect("ejection");
        assert_eq!(ejection.removed_head.as_deref(), Some(REMOVED_8912));
        assert_eq!(ejection.new_head_basis, NewHeadBasis::RemovedHeadSha);
        assert!(ejection.new_head_since);
        assert_eq!(
            report.state,
            PrQueueState::Ejected {
                reason: "failed_checks".to_owned(),
                at: Some("2026-09-27T05:52:04Z".to_owned()),
                new_head_since_removal: true,
                requeues_without_new_head: 0,
            }
        );
    }

    #[test]
    fn the_head_the_queue_removed_is_still_the_same_head() {
        let mut value = fixture(BACKDATED_FIXTURE);
        pr_mut(&mut value)["headRefOid"] = Value::from(REMOVED_8912);
        let ejection = explain_pr_queue_state(&value)
            .last_ejection
            .expect("ejection");
        assert_eq!(ejection.new_head_basis, NewHeadBasis::RemovedHeadSha);
        assert!(!ejection.new_head_since);
        // Even with a push of that same head after the removal: equal SHAs
        // are the same head whatever the timeline says.
        nodes_mut(&mut value).push(serde_json::json!({
            "__typename": "HeadRefForcePushedEvent",
            "afterCommit": {"oid": REMOVED_8912},
        }));
        assert!(matches!(
            classify_pr_queue_state(&value),
            PrQueueState::Ejected {
                new_head_since_removal: false,
                ..
            }
        ));
    }

    #[test]
    fn force_push_to_an_older_sha_is_a_new_head() {
        let mut value = fixture(BACKDATED_FIXTURE);
        let older = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";
        pr_mut(&mut value)["headRefOid"] = Value::from(older);
        nodes_mut(&mut value).push(serde_json::json!({
            "__typename": "HeadRefForcePushedEvent",
            "afterCommit": {"oid": older},
        }));
        let ejection = explain_pr_queue_state(&value)
            .last_ejection
            .expect("ejection");
        assert_eq!(ejection.removed_head.as_deref(), Some(REMOVED_8912));
        assert!(ejection.new_head_since);
    }

    #[test]
    fn missing_before_commit_falls_back_to_pushes_and_fails_closed() {
        let mut value = fixture(BACKDATED_FIXTURE);
        let index = removal_index(&value);
        nodes_mut(&mut value)[index]
            .as_object_mut()
            .expect("removal")
            .remove("beforeCommit");
        // No removed head, and the back-dated fix sorts before the removal:
        // no evidence of a new head, so none is assumed and a re-arm is refused.
        let ejection = explain_pr_queue_state(&value)
            .last_ejection
            .expect("ejection");
        assert_eq!(ejection.removed_head, None);
        assert_eq!(ejection.new_head_basis, NewHeadBasis::NoEvidence);
        assert!(!ejection.new_head_since);
        // A later push of some other commit is still not the current head.
        nodes_mut(&mut value).push(serde_json::json!({
            "__typename": "PullRequestCommit",
            "commit": {"oid": PUSHED_HEAD},
        }));
        assert!(
            !explain_pr_queue_state(&value)
                .last_ejection
                .expect("ejection")
                .new_head_since
        );
        // A later push of the current head is.
        pr_mut(&mut value)["headRefOid"] = Value::from(PUSHED_HEAD);
        let ejection = explain_pr_queue_state(&value)
            .last_ejection
            .expect("ejection");
        assert_eq!(ejection.new_head_basis, NewHeadBasis::PushAfterRemoval);
        assert!(ejection.new_head_since);
    }

    #[test]
    fn a_merge_group_parent_this_pr_never_had_is_not_trusted() {
        let mut value = fixture(BACKDATED_FIXTURE);
        let index = removal_index(&value);
        nodes_mut(&mut value)[index]["beforeCommit"]["parents"]["nodes"][1]["oid"] =
            Value::from(PUSHED_HEAD);
        let ejection = explain_pr_queue_state(&value)
            .last_ejection
            .expect("ejection");
        assert_eq!(ejection.removed_head, None);
        assert_eq!(ejection.new_head_basis, NewHeadBasis::NoEvidence);
        assert!(!ejection.new_head_since);
    }

    #[test]
    fn requeue_count_compares_removed_heads_not_timeline_position() {
        // A failed_checks removal of A, a back-dated fix B that sorts before
        // it, a re-add, and a second removal naming B: the re-add enqueued a
        // new head, which timeline order alone would count as a same-head
        // re-enqueue.
        let a = REMOVED_8912;
        let b = FIX_8912;
        let removal = |head: &str| {
            serde_json::json!({
                "__typename": "RemovedFromMergeQueueEvent",
                "reason": "failed_checks",
                "createdAt": "2026-09-27T05:52:04Z",
                "beforeCommit": {"oid": "ffff", "parents": {"nodes": [{"oid": "base"}, {"oid": head}]}},
            })
        };
        let build = |second: &str| {
            serde_json::json!({"data": {"repository": {"pullRequest": {
                "number": 1, "state": "OPEN", "headRefOid": b,
                "isInMergeQueue": false, "mergeQueueEntry": null, "autoMergeRequest": null,
                "timelineItems": {"pageInfo": {"hasPreviousPage": false}, "nodes": [
                    {"__typename": "PullRequestCommit", "commit": {"oid": a}},
                    {"__typename": "AddedToMergeQueueEvent", "createdAt": "t0"},
                    {"__typename": "PullRequestCommit", "commit": {"oid": b}},
                    removal(a),
                    {"__typename": "AddedToMergeQueueEvent", "createdAt": "t1"},
                    removal(second),
                ]},
            }}}})
        };
        assert_eq!(
            explain_pr_queue_state(&build(b)).requeues_without_new_head,
            0
        );
        // Control: the same shape with the second removal naming A again is a
        // same-head re-enqueue.
        assert_eq!(
            explain_pr_queue_state(&build(a)).requeues_without_new_head,
            1
        );
    }

    #[test]
    fn node_lookup_shape_is_accepted() {
        let value = fixture("pr_queued.json");
        let node =
            serde_json::json!({"data": {"node": value["data"]["repository"]["pullRequest"]}});
        assert_eq!(classify_pr_queue_state(&node).class(), "queued");
    }
}
