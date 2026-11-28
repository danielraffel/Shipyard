//! An approval counts only for the base it was given against.
//!
//! A reviewer judges a pull request's diff against its base. When child C is
//! stacked on parent P (C's base is P's branch), an APPROVE of C's exact head
//! covers only C's own commits. If P then closes unmerged and C is retargeted
//! to the default branch with its head unchanged, the same APPROVE would land
//! P's commits, which nobody reviewed for that base. So an APPROVE posted at
//! or before the pull request's latest base change does not count: the gate
//! decides as if it had never been posted, and reports
//! `approval_void:base_changed` when that leaves the head unapproved. A new
//! APPROVE posted after the change counts as usual. A HOLD is never dropped.
//!
//! Base changes come from the pull request's GraphQL timeline:
//! `BaseRefChangedEvent` (a retarget by hand or through the API) and
//! `AutomaticBaseChangeSucceededEvent` (GitHub retargeting a stacked child
//! after its parent merged and the parent's branch was deleted). A manual
//! change always voids. An automatic one voids too, because the child may
//! carry the parent's commits at a head other than the one that merged,
//! unless it is provably safe ([`automatic_change_safe`]): exactly one PR
//! from the old base into the new base merged before the event, and its
//! final head is an ancestor of (or equal to) the child's head (compare
//! status `ahead` or `identical`). Then the diff the reviewer approved,
//! against the parent's head, is exactly the child's own content on top of
//! what landed, so the approval keeps counting across that change. Anything
//! unreadable or ambiguous voids. The floor is the newest change that is not
//! safe.
//!
//! The read is needed only when some APPROVE record exists, and it fails
//! closed: an answer with errors, a further page, a count that does not match
//! the nodes, an unknown event, an unreadable time, or a latest change whose
//! new base is not the pull request's current base is an error, never "no
//! change". The queue-arm guard (`scripts/ghapp_queue_arm_guard.py`,
//! `BASE_CHANGE_QUERY` and `base_changes`) applies the same rule, and the
//! `review-verdict` status takes its decision from that guard.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use super::{RunGh, Verdict, at, marker_verdict};

/// The timeline read. Kept byte-equal to `BASE_CHANGE_QUERY` in
/// `scripts/ghapp_queue_arm_guard.py` (a test reads both).
pub const BASE_CHANGE_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){baseRefName timelineItems(itemTypes:[BASE_REF_CHANGED_EVENT,AUTOMATIC_BASE_CHANGE_SUCCEEDED_EVENT],first:100){filteredCount pageInfo{hasNextPage} nodes{__typename ... on BaseRefChangedEvent{createdAt previousRefName currentRefName} ... on AutomaticBaseChangeSucceededEvent{createdAt oldBase newBase}}}}}}";

/// The merged parent read for an automatic change. Kept byte-equal to
/// `MERGED_PARENT_QUERY` in `scripts/ghapp_queue_arm_guard.py`.
pub const MERGED_PARENT_QUERY: &str = "query($owner:String!,$name:String!,$head:String!,$base:String!){repository(owner:$owner,name:$name){pullRequests(headRefName:$head,baseRefName:$base,states:[MERGED],first:20){totalCount nodes{number mergedAt headRefOid}}}}";

/// A [`BASE_CHANGE_QUERY`] answer with no base change, for test fakes.
#[cfg(test)]
pub(crate) const NO_BASE_CHANGES: &str = r#"{"data":{"repository":{"pullRequest":{"baseRefName":"main","timelineItems":{"filteredCount":0,"pageInfo":{"hasNextPage":false},"nodes":[]}}}}}"#;

/// The reason in the `approval_void:<reason>` token.
pub const BASE_CHANGED: &str = "base_changed";

/// One change of a pull request's base.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BaseChange {
    /// When it happened, as GitHub spells it.
    pub at: String,
    /// The base before.
    pub from: String,
    /// The base after.
    pub to: String,
    /// GitHub retargeted it after the old base merged.
    pub automatic: bool,
}

impl BaseChange {
    /// One phrase for a report.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "its base changed from {} to {} at {}{}",
            self.from,
            self.to,
            self.at,
            if self.automatic {
                " (GitHub retargeted it after the old base merged)"
            } else {
                ""
            }
        )
    }
}

fn text<'a>(node: &'a Value, key: &str) -> Result<&'a str, String> {
    node.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("a base change carries no {key}"))
}

/// Every base change in a [`BASE_CHANGE_QUERY`] answer, oldest first.
///
/// # Errors
///
/// When the answer does not prove it is the whole timeline (see the module
/// docs): an incomplete read is never "no change".
pub fn base_changes(answer: &Value) -> Result<Vec<BaseChange>, String> {
    if answer.get("errors").is_some_and(|errors| !errors.is_null()) {
        return Err(format!("the timeline read failed: {}", answer["errors"]));
    }
    let pull = answer
        .pointer("/data/repository/pullRequest")
        .filter(|pull| pull.is_object())
        .ok_or("the timeline read names no pull request")?;
    let base = pull
        .get("baseRefName")
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty())
        .ok_or("the timeline read names no current base")?;
    let items = pull
        .get("timelineItems")
        .ok_or("the timeline read carries no timeline")?;
    if items
        .pointer("/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        != Some(false)
    {
        return Err("the base-change timeline has a further page".to_owned());
    }
    let nodes = items
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or("the base-change timeline carries no nodes")?;
    if items.get("filteredCount").and_then(Value::as_u64) != Some(nodes.len() as u64) {
        return Err("the base-change timeline count does not match its nodes".to_owned());
    }
    let mut changes = Vec::with_capacity(nodes.len());
    for node in nodes {
        let (from, to, automatic) = match node.get("__typename").and_then(Value::as_str) {
            Some("BaseRefChangedEvent") => (
                text(node, "previousRefName")?,
                text(node, "currentRefName")?,
                false,
            ),
            Some("AutomaticBaseChangeSucceededEvent") => {
                (text(node, "oldBase")?, text(node, "newBase")?, true)
            }
            other => return Err(format!("the base-change timeline carries a {other:?} node")),
        };
        let at = text(node, "createdAt")?;
        let when = DateTime::parse_from_rfc3339(at)
            .map_err(|error| format!("a base change time is unreadable: {error}"))?
            .with_timezone(&Utc);
        changes.push((
            when,
            BaseChange {
                at: at.to_owned(),
                from: from.to_owned(),
                to: to.to_owned(),
                automatic,
            },
        ));
    }
    changes.sort_by_key(|(when, _)| *when);
    if let Some((_, last)) = changes.last()
        && last.to != base
    {
        return Err(format!(
            "the latest base change moved it to {} but its base is {base}",
            last.to
        ));
    }
    Ok(changes.into_iter().map(|(_, change)| change).collect())
}

/// Read `repo#pr`'s base changes.
///
/// # Errors
///
/// When the read fails or is incomplete ([`base_changes`]).
pub fn read_base_changes(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
) -> Result<Vec<BaseChange>, String> {
    let (owner, name) = repo
        .split_once('/')
        .filter(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| format!("{repo} is not owner/name"))?;
    let raw = run_gh(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={BASE_CHANGE_QUERY}"),
        "-f".to_owned(),
        format!("owner={owner}"),
        "-f".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("number={pr}"),
    ])?;
    let answer: Value = serde_json::from_str(raw.trim())
        .map_err(|error| format!("the timeline read is not JSON: {error}"))?;
    base_changes(&answer)
}

/// Whether `record` is an approval of any head: an `APPROVED` review, or a
/// comment whose verdict line approves.
pub(super) fn is_approval(record: &Value) -> bool {
    if let Some(state) = record.get("state").and_then(Value::as_str) {
        return state.eq_ignore_ascii_case("APPROVED");
    }
    record
        .get("body")
        .and_then(Value::as_str)
        .and_then(marker_verdict)
        .is_some_and(|(verdict, _)| verdict == Verdict::Approve)
}

/// `records` without the approvals posted at or before `change` (or with no
/// readable time), and the approvals dropped.
pub(super) fn without_approvals_through(
    records: Vec<Value>,
    change: &BaseChange,
) -> (Vec<Value>, Vec<Value>) {
    let floor = DateTime::parse_from_rfc3339(&change.at)
        .map_or(DateTime::<Utc>::MAX_UTC, |at| at.with_timezone(&Utc));
    records
        .into_iter()
        .partition(|record| !is_approval(record) || at(record).is_some_and(|when| when > floor))
}

/// The final head of the one parent pull request whose merge made GitHub
/// retarget `change` (head branch `change.from`, into `change.to`, merged at
/// or before the change), from a [`MERGED_PARENT_QUERY`] answer. `None` when
/// the answer has errors, is incomplete or malformed, or names no such pull
/// request or several.
#[must_use]
pub fn merged_parent_head(answer: &Value, change: &BaseChange) -> Option<String> {
    if answer.get("errors").is_some_and(|errors| !errors.is_null()) {
        return None;
    }
    let pulls = answer.pointer("/data/repository/pullRequests")?;
    let nodes = pulls.get("nodes")?.as_array()?;
    if pulls.get("totalCount")?.as_u64()? != nodes.len() as u64 {
        return None;
    }
    let event = DateTime::parse_from_rfc3339(&change.at).ok()?;
    let mut candidates = Vec::new();
    for node in nodes {
        let merged = DateTime::parse_from_rfc3339(node.get("mergedAt")?.as_str()?).ok()?;
        let head = node
            .get("headRefOid")?
            .as_str()
            .filter(|head| super::is_api_sha(head))?;
        if merged <= event {
            candidates.push(head.to_ascii_lowercase());
        }
    }
    match candidates.as_slice() {
        [head] => Some(head.clone()),
        _ => None,
    }
}

/// Whether a compare status of `parent...child` proves the parent's head is
/// an ancestor of, or equal to, the child's head.
#[must_use]
pub fn ancestor_status(status: &str) -> bool {
    matches!(status, "ahead" | "identical")
}

/// The newest change in `changes` that is not `safe`: the floor approvals
/// must be newer than. A manual change is never safe.
pub fn floor_change(
    changes: &[BaseChange],
    mut safe: impl FnMut(&BaseChange) -> bool,
) -> Option<BaseChange> {
    changes
        .iter()
        .rev()
        .find(|change| !(change.automatic && safe(change)))
        .cloned()
}

/// Whether the automatic `change` is provably safe for a child at `head`
/// ([`merged_parent_head`], then `compare` [`ancestor_status`]). Any read
/// that fails is not safe.
pub fn automatic_change_safe(
    run_gh: RunGh<'_>,
    repo: &str,
    head: &str,
    change: &BaseChange,
) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let answer = run_gh(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={MERGED_PARENT_QUERY}"),
        "-f".to_owned(),
        format!("owner={owner}"),
        "-f".to_owned(),
        format!("name={name}"),
        "-f".to_owned(),
        format!("head={}", change.from),
        "-f".to_owned(),
        format!("base={}", change.to),
    ])
    .ok()
    .and_then(|raw| serde_json::from_str::<Value>(raw.trim()).ok());
    let Some(parent) = answer.and_then(|answer| merged_parent_head(&answer, change)) else {
        return false;
    };
    run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/compare/{parent}...{head}"),
        "--jq".to_owned(),
        ".status".to_owned(),
    ])
    .is_ok_and(|status| ancestor_status(status.trim()))
}

/// Reviews, comments, and the base change that voided an approval.
pub(super) type Judged = (Vec<Value>, Vec<Value>, Option<BaseChange>);

/// `(reviews, comments, voided)`: the records without every APPROVE posted
/// at or before `repo#pr`'s newest base change that is not provably safe
/// ([`floor_change`]), and that change when a
/// counting APPROVE was among those dropped. The timeline is read only when
/// some APPROVE exists.
///
/// # Errors
///
/// When the timeline cannot be read in full ([`base_changes`]).
#[allow(clippy::too_many_arguments)] // The gate's records and identity, as `evaluate_events`.
pub(super) fn void_approvals(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: &str,
    reviews: Vec<Value>,
    comments: Vec<Value>,
    policy: &super::ApprovalPolicy,
    author: Option<&str>,
) -> Result<Judged, String> {
    if !reviews.iter().chain(&comments).any(is_approval) {
        return Ok((reviews, comments, None));
    }
    let changes = read_base_changes(run_gh, repo, pr)
        .map_err(|error| format!("PR #{pr}'s base changes are unreadable: {error}"))?;
    let Some(change) = floor_change(&changes, |change| {
        automatic_change_safe(run_gh, repo, head, change)
    }) else {
        return Ok((reviews, comments, None));
    };
    let (reviews, mut dropped) = without_approvals_through(reviews, &change);
    let (comments, dropped_comments) = without_approvals_through(comments, &change);
    dropped.extend(dropped_comments);
    let voided = dropped
        .iter()
        .any(|record| super::verdict_counts(record, Verdict::Approve, policy, author));
    Ok((reviews, comments, voided.then_some(change)))
}

#[cfg(test)]
#[path = "base_change_tests.rs"]
mod tests;
