//! Why a head the approval gate refuses is refused.
//!
//! [`super::HeadGate::NotApproved`] covers several different states: no
//! reviewer has spoken, a reviewer holds the head, an approval exists but does
//! not count (an `unattributed` session stamp, a signer no list names, the
//! PR's own author), an approval predates the head, or the only approval names
//! an older head. Each needs a different fix by a different party, so the
//! steward reports them apart. The diagnosis reads only the records the gate
//! already read.

use serde::Serialize;
use serde_json::Value;

use super::{
    ApprovalPolicy, HeadGate, Verdict, app_approval_refusal_reason, approver_allowed, head_hold,
    hold_signer, is_fleet_author, login, marker_verdict, normalized_login, reviewer_allowed,
};

/// Why the approval gate refuses a head.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalBlocker {
    /// No verdict of any kind names this head.
    NoVerdict,
    /// A counting reviewer holds this head; only a new push can be approved.
    Held {
        /// The `reviewer:` signer of the HOLD, else its login.
        holder: String,
        /// `comment` or `review`.
        record: &'static str,
        /// The comment or review id carrying the HOLD.
        id: Option<u64>,
        /// When the HOLD was posted.
        at: Option<String>,
    },
    /// An approval names this head but does not count.
    Excluded {
        /// Stable machine token for the reason (`unattributed`,
        /// `uncounted_signer`, `pr_author`, `predates_head`, ...).
        reason: &'static str,
        /// The reason in words.
        detail: String,
        /// The excluded record's id.
        id: Option<u64>,
        /// When the excluded approval was posted.
        at: Option<String>,
    },
    /// A counting approval exists, but for an earlier head.
    StaleHead {
        /// The head the newest counting approval names.
        approved_head: String,
        /// When that approval was posted.
        at: Option<String>,
    },
    /// The merge queue removed this head too often and no approval followed.
    EjectionCap {
        /// When the last removal happened.
        at: Option<String>,
    },
    /// The deciding approval was posted by the agent now arming it.
    SelfApproved {
        /// The agent that both approved and armed.
        agent: String,
    },
}

impl ApprovalBlocker {
    /// One stable machine token: `no_verdict`, `held`,
    /// `approval_excluded:<reason>`, or `ejection_cap`.
    #[must_use]
    pub fn token(&self) -> String {
        match self {
            Self::NoVerdict => "no_verdict".to_owned(),
            Self::Held { .. } => "held".to_owned(),
            Self::Excluded { reason, .. } => format!("approval_excluded:{reason}"),
            Self::StaleHead { .. } => "approval_excluded:stale_head".to_owned(),
            Self::EjectionCap { .. } => "ejection_cap".to_owned(),
            Self::SelfApproved { .. } => "approval_excluded:self_approved".to_owned(),
        }
    }

    /// When the blocking state began, when a record says so.
    #[must_use]
    pub fn since(&self) -> Option<&str> {
        match self {
            Self::Held { at, .. }
            | Self::Excluded { at, .. }
            | Self::StaleHead { at, .. }
            | Self::EjectionCap { at } => at.as_deref(),
            Self::NoVerdict | Self::SelfApproved { .. } => None,
        }
    }
}

/// The review and comment records the gate judged.
pub(super) struct Records<'a> {
    /// The head being judged.
    pub head: &'a str,
    /// Reviews as the gate read them.
    pub reviews: &'a [Value],
    /// Comments the gate judged: those naming this head that predate its
    /// commit are already dropped.
    pub comments: &'a [Value],
    /// Every comment, before that drop.
    pub all_comments: &'a [Value],
}

/// Why `gate` refuses the head, or `None` when it allows it.
pub(super) fn diagnose(
    gate: &HeadGate,
    records: &Records<'_>,
    policy: &ApprovalPolicy,
    author: Option<&str>,
) -> Option<ApprovalBlocker> {
    match gate {
        HeadGate::NotRequired | HeadGate::Approved { .. } => None,
        HeadGate::SelfApproved { agent, .. } => Some(ApprovalBlocker::SelfApproved {
            agent: agent.clone(),
        }),
        HeadGate::EjectionCap { ejections, .. } => Some(ApprovalBlocker::EjectionCap {
            at: ejections.last().and_then(|ejection| ejection.at.clone()),
        }),
        HeadGate::NotApproved { .. } => Some(not_approved(records, policy, author)),
    }
}

/// Why a head with no counting approval has none, from the gate's records.
#[must_use]
pub(super) fn not_approved(
    records: &Records<'_>,
    policy: &ApprovalPolicy,
    author: Option<&str>,
) -> ApprovalBlocker {
    let head = records.head;
    if let Some(hold) = head_hold(head, records.reviews, records.comments, policy, author) {
        return ApprovalBlocker::Held {
            holder: hold_signer(hold),
            record: if hold.get("state").is_some() {
                "review"
            } else {
                "comment"
            },
            id: hold.get("id").and_then(Value::as_u64),
            at: at_text(hold),
        };
    }
    let judged = newest_first(records.reviews.iter().chain(records.comments));
    if let Some((record, (reason, detail))) = judged.iter().find_map(|record| {
        (approval_head(record).is_some_and(|named| named.eq_ignore_ascii_case(head)))
            .then(|| exclusion(record, policy, author).map(|why| (*record, why)))
            .flatten()
    }) {
        return ApprovalBlocker::Excluded {
            reason,
            detail,
            id: record.get("id").and_then(Value::as_u64),
            at: at_text(record),
        };
    }
    let judged_ids: Vec<Option<u64>> = records
        .comments
        .iter()
        .map(|comment| comment.get("id").and_then(Value::as_u64))
        .collect();
    if let Some(early) = newest_first(records.all_comments.iter())
        .into_iter()
        .find(|comment| {
            approval_head(comment).is_some_and(|named| named.eq_ignore_ascii_case(head))
                && !judged_ids.contains(&comment.get("id").and_then(Value::as_u64))
        })
    {
        return ApprovalBlocker::Excluded {
            reason: "predates_head",
            detail: "the approval naming this head was posted before the head's commit".to_owned(),
            id: early.get("id").and_then(Value::as_u64),
            at: at_text(early),
        };
    }
    // An approval of an earlier head from someone whose verdicts count at
    // all: whether it would also have passed the approver rule does not
    // matter, this head needs its own.
    if let Some((record, approved_head)) = judged.iter().find_map(|record| {
        let named = approval_head(record)?;
        (!named.eq_ignore_ascii_case(head) && reviewer_allowed(record, policy, author))
            .then_some((*record, named))
    }) {
        return ApprovalBlocker::StaleHead {
            approved_head,
            at: at_text(record),
        };
    }
    ApprovalBlocker::NoVerdict
}

/// Why an approval record does not count, with a stable token.
fn exclusion(
    record: &Value,
    policy: &ApprovalPolicy,
    author: Option<&str>,
) -> Option<(&'static str, String)> {
    let who = login(record).unwrap_or_default();
    if !reviewer_allowed(record, policy, author) {
        let reason = if is_fleet_author(who)
            && record.get("type").and_then(Value::as_str) != Some("Bot")
        {
            (
                "impersonated_app",
                format!("{who} is a user account, not the fleet App"),
            )
        } else if author.is_some_and(|author| normalized_login(author) == normalized_login(who)) {
            ("pr_author", format!("{who} wrote the pull request"))
        } else if policy.reviewer_logins.is_empty() {
            (
                "untrusted_reviewer",
                format!("{who} is neither the fleet App nor a trusted collaborator"),
            )
        } else {
            (
                "unlisted_reviewer",
                format!("{who} is not a listed reviewer"),
            )
        };
        return Some(reason);
    }
    if approver_allowed(record, policy) {
        return None;
    }
    if is_fleet_author(who) {
        return app_approval_refusal_reason(record, policy);
    }
    Some((
        "unlisted_human",
        format!("{who} is not a listed human reviewer"),
    ))
}

/// The head an approval record names: an APPROVED review's commit, or an
/// APPROVE marker's head.
fn approval_head(record: &Value) -> Option<String> {
    if let Some(state) = record.get("state").and_then(Value::as_str) {
        return state
            .eq_ignore_ascii_case("APPROVED")
            .then(|| record.get("commit_id").and_then(Value::as_str))
            .flatten()
            .map(str::to_ascii_lowercase);
    }
    let (verdict, head) = marker_verdict(record.get("body").and_then(Value::as_str)?)?;
    (verdict == Verdict::Approve).then_some(head)
}

fn newest_first<'a>(records: impl Iterator<Item = &'a Value>) -> Vec<&'a Value> {
    let mut records: Vec<&Value> = records.collect();
    // Stable: records without a time keep their relative (oldest-first) order
    // and sort after every timed record.
    records.sort_by_key(|record| std::cmp::Reverse(at_text(record)));
    records
}

fn at_text(record: &Value) -> Option<String> {
    record.get("at").and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
#[path = "blocker_tests.rs"]
mod tests;
