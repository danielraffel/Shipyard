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
//! ## Why history is read in timeline order
//!
//! `RemovedFromMergeQueueEvent.beforeCommit` is not a reliable witness of the
//! head at removal time, and commit dates are author-controlled. "Was a new
//! head pushed after this ejection" is therefore answered by a
//! `PullRequestCommit` or `HeadRefForcePushedEvent` item appearing *after* the
//! removal in the timeline's own order.
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
pub const PR_QUEUE_STATE_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){number state headRefOid isInMergeQueue mergeQueueEntry{state position} autoMergeRequest{enabledAt} timelineItems(last:100,itemTypes:[PULL_REQUEST_COMMIT,HEAD_REF_FORCE_PUSHED_EVENT,ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT,AUTO_MERGE_ENABLED_EVENT,AUTO_MERGE_DISABLED_EVENT,MERGED_EVENT]){pageInfo{hasPreviousPage} nodes{__typename ... on PullRequestCommit{commit{oid}} ... on HeadRefForcePushedEvent{createdAt afterCommit{oid}} ... on AddedToMergeQueueEvent{createdAt actor{login}} ... on RemovedFromMergeQueueEvent{createdAt reason actor{login}} ... on AutoMergeEnabledEvent{createdAt actor{login}} ... on AutoMergeDisabledEvent{createdAt reason actor{login}} ... on MergedEvent{createdAt}}}}}}";

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
    /// Whether a commit or force-push follows it in timeline order.
    pub new_head_since: bool,
    /// Timeline index of the removal item.
    pub timeline_index: usize,
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
        timeline_complete: None,
        facts,
    }
}

fn is_new_head(typename: &str) -> bool {
    matches!(typename, "PullRequestCommit" | "HeadRefForcePushedEvent")
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

    // Walk the timeline once, in its own order.
    let mut requeues = 0_u32;
    let mut requeue_indices = Vec::new();
    let mut hazard_pending = false;
    let mut last_removal: Option<(usize, String, Option<String>)> = None;
    let mut last_new_head: Option<usize> = None;
    for (index, node) in nodes.iter().enumerate() {
        let typename = node.get("__typename").and_then(Value::as_str).unwrap_or("");
        if is_new_head(typename) {
            hazard_pending = false;
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
                hazard_pending = same_head_requeue_cascades(&reason);
                let at = node
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                last_removal = Some((index, reason, at));
            }
            "AddedToMergeQueueEvent" => {
                if hazard_pending {
                    requeues += 1;
                    requeue_indices.push(index);
                }
                hazard_pending = false;
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
             failed_checks/merge_conflict RemovedFromMergeQueueEvent, no commit or force-push \
             between)"
        ),
    );

    let last_removal_seen = last_removal.as_ref().map(|(index, _, _)| *index);
    let last_ejection = last_removal
        .filter(|(_, reason, _)| !reason.eq_ignore_ascii_case("merged"))
        .map(|(index, reason, at)| Ejection {
            new_head_since: last_new_head.is_some_and(|head| head > index),
            reason,
            at,
            timeline_index: index,
        });
    if let Some(ejection) = &last_ejection {
        fact(
            &mut facts,
            "last_ejection",
            serde_json::json!({
                "reason": ejection.reason,
                "at": ejection.at,
                "new_head_since": ejection.new_head_since,
            }),
            &format!(
                "timelineItems.nodes[{}] (RemovedFromMergeQueueEvent; new head = a later \
                 PullRequestCommit/HeadRefForcePushedEvent{})",
                ejection.timeline_index,
                last_new_head
                    .filter(|head| *head > ejection.timeline_index)
                    .map_or_else(String::new, |head| format!(" at nodes[{head}]"))
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
        timeline_complete,
        facts,
    }
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
        assert_eq!(checked, 10);
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
        value["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]
            .as_array_mut()
            .expect("nodes")
            .push(serde_json::json!({"__typename": "HeadRefForcePushedEvent"}));
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

    #[test]
    fn node_lookup_shape_is_accepted() {
        let value = fixture("pr_queued.json");
        let node =
            serde_json::json!({"data": {"node": value["data"]["repository"]["pullRequest"]}});
        assert_eq!(classify_pr_queue_state(&node).class(), "queued");
    }
}
