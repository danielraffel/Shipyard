//! A pull request's counting verdicts on every head, with their times: what
//! the carried-commit guard compares to decide whether a HOLD on a shared
//! commit is newer than the candidate's own exact-head approval.
//!
//! Only server-set times are read: a review's `submitted_at`, a comment's
//! `created_at`, and for a comment also its `updated_at`, so an edit can never
//! date a HOLD earlier than its last change. Author-set metadata (labels,
//! commit dates, PR body text) is never read here.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    ApprovalPolicy, COMMENT_RECORD_JQ, REVIEW_RECORD_JQ, RunGh, Verdict, arming_agent, at,
    comment_edit_stamps, comment_ever_edited, marker_verdict, pr_author, read_records,
    stamped_agent, verdict_counts, verdict_records_checked,
};

/// One counting HOLD, on any head of the pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HoldRecord {
    /// The head the HOLD names.
    pub head: String,
    /// When it was posted, or last edited if later.
    pub at: DateTime<Utc>,
    /// `comment` or `review`.
    pub record: &'static str,
    /// The comment or review id.
    pub id: Option<u64>,
}

/// One counting APPROVE that names an exact head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApproveRecord {
    /// The head it names.
    pub head: String,
    /// When it was posted.
    pub at: DateTime<Utc>,
    /// The comment carrying it, for a marker; `None` for a review.
    pub comment: Option<Value>,
}

/// A pull request's counting verdicts, every head.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VerdictHistory {
    /// Every counting HOLD.
    pub holds: Vec<HoldRecord>,
    /// Every counting, unedited exact-head APPROVE.
    pub approvals: Vec<ApproveRecord>,
}

impl VerdictHistory {
    /// When `head` was last approved by a counting, unedited exact-head
    /// APPROVE. `None` when no such record names it (an approval carried from
    /// another head, or a rollup, has no time of its own here).
    #[must_use]
    pub fn approved_at(&self, head: &str) -> Option<DateTime<Utc>> {
        self.approvals
            .iter()
            .filter(|approve| approve.head.eq_ignore_ascii_case(head))
            .map(|approve| approve.at)
            .max()
    }

    /// Drop the approvals of `head` that GitHub's GraphQL edit history says
    /// were edited, newest first, until the newest left is a review or an
    /// unedited comment: the one [`Self::approved_at`] then dates `head` by.
    /// The gate never counts an edited approval, so neither does the date.
    ///
    /// # Errors
    ///
    /// When an approval's edit history cannot be read.
    pub fn without_edited_approvals(
        mut self,
        run_gh: RunGh<'_>,
        head: &str,
    ) -> Result<Self, String> {
        loop {
            let newest = self
                .approvals
                .iter()
                .enumerate()
                .filter(|(_, approve)| approve.head.eq_ignore_ascii_case(head))
                .max_by_key(|(_, approve)| approve.at)
                .map(|(index, approve)| (index, approve.comment.clone()));
            match newest {
                Some((index, Some(comment))) if comment_ever_edited(run_gh, &comment)? => {
                    self.approvals.remove(index);
                }
                _ => return Ok(self),
            }
        }
    }
}

/// Read `pr`'s counting verdicts on every head: HOLD markers and
/// `CHANGES_REQUESTED` reviews from a counting reviewer, and counting APPROVE
/// markers and `APPROVED` reviews. `author` is the pull request's author when
/// known (read otherwise, as the gate does), never counted as a reviewer.
///
/// # Errors
///
/// When the records cannot be read or judged, or a counting HOLD's time is
/// unreadable (it could be newer than anything). An APPROVE whose time is
/// unreadable is dropped: it cannot prove it is newer than a HOLD.
pub fn verdict_history(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    policy: &ApprovalPolicy,
    author: Option<&str>,
) -> Result<VerdictHistory, String> {
    let author = pr_author(run_gh, repo, pr, policy, author)?;
    let reviews = read_records(
        run_gh,
        &format!("repos/{repo}/pulls/{pr}/reviews"),
        REVIEW_RECORD_JQ,
    )?;
    let comments = read_records(
        run_gh,
        &format!("repos/{repo}/issues/{pr}/comments"),
        COMMENT_RECORD_JQ,
    )?;
    history_from_records(pr, &reviews, &comments, policy, author.as_deref())
}

/// [`verdict_history`] over records already read. Pure.
///
/// # Errors
///
/// As [`verdict_history`].
pub fn history_from_records(
    pr: u64,
    reviews: &[Value],
    comments: &[Value],
    policy: &ApprovalPolicy,
    author: Option<&str>,
) -> Result<VerdictHistory, String> {
    verdict_records_checked(reviews, comments, policy)
        .map_err(|error| format!("PR #{pr}'s verdicts are unreadable: {error}"))?;
    let mut history = VerdictHistory::default();
    let id = |record: &Value| record.get("id").and_then(Value::as_u64);
    for review in reviews {
        let state = review.get("state").and_then(Value::as_str).unwrap_or("");
        let Some(head) = review.get("commit_id").and_then(Value::as_str) else {
            continue;
        };
        if state.eq_ignore_ascii_case("CHANGES_REQUESTED")
            && verdict_counts(review, Verdict::Hold, policy, author)
        {
            let when = at(review).ok_or_else(|| {
                format!(
                    "PR #{pr}'s HOLD review {:?} has no readable time",
                    id(review)
                )
            })?;
            history.holds.push(HoldRecord {
                head: head.to_ascii_lowercase(),
                at: when,
                record: "review",
                id: id(review),
            });
        } else if state.eq_ignore_ascii_case("APPROVED")
            && verdict_counts(review, Verdict::Approve, policy, author)
            && let Some(when) = at(review)
        {
            history.approvals.push(ApproveRecord {
                head: head.to_ascii_lowercase(),
                at: when,
                comment: None,
            });
        }
    }
    for comment in comments {
        let Some((verdict, head)) = comment
            .get("body")
            .and_then(Value::as_str)
            .and_then(marker_verdict)
        else {
            continue;
        };
        if !verdict_counts(comment, verdict, policy, author) {
            continue;
        }
        match verdict {
            Verdict::Hold => {
                // An edit dates the HOLD at its last change. A record with no
                // `updated_at` key (the gate's reads always carry one) is
                // dated by its posting time.
                let when = match comment_edit_stamps(comment).map_err(|error| {
                    format!("PR #{pr}'s HOLD comment {:?}: {error}", id(comment))
                })? {
                    Some((posted, updated)) => posted.max(updated),
                    None => at(comment).ok_or_else(|| {
                        format!(
                            "PR #{pr}'s HOLD comment {:?} has no readable time",
                            id(comment)
                        )
                    })?,
                };
                history.holds.push(HoldRecord {
                    head,
                    at: when,
                    record: "comment",
                    id: id(comment),
                });
            }
            Verdict::Approve => {
                // `verdict_counts` already refuses an approval REST shows as
                // edited; an approval stamped by the agent now arming is a
                // self-approval the gate refuses, so it never dates a head.
                let mine = arming_agent();
                let stamped = comment
                    .get("body")
                    .and_then(Value::as_str)
                    .and_then(stamped_agent);
                if mine.is_some() && stamped == mine {
                    continue;
                }
                if let Some(when) = at(comment) {
                    history.approvals.push(ApproveRecord {
                        head,
                        at: when,
                        comment: Some(comment.clone()),
                    });
                }
            }
        }
    }
    Ok(history)
}

#[cfg(test)]
mod tests;
