//! What a merge-queue admission mutation actually did, read from GitHub.
//!
//! Two questions with one rule: only GitHub's own answer settles a mutation.
//!
//! * [`definitive_refusal`] reads the answer to the steward's own enqueue. A
//!   typed GraphQL refusal (`UNPROCESSABLE`, `FORBIDDEN`, `NOT_FOUND`) with no
//!   mutation payload means GitHub declined the request; nothing was queued.
//!   It is recorded `rejected` with GitHub's message and reported with its own
//!   `blocked_on` token, never `steward_error`. A transport failure or timeout
//!   is not an answer and stays uncertain.
//! * [`reconcile`] settles an uncertain admission recorded on an earlier pass
//!   by re-reading the pull request: merged, or queued at the recorded head,
//!   is `accepted`; no queue entry, no auto-merge request and not merged is
//!   `rejected`. Anything else stays uncertain, and stays contained to that
//!   one pull request.

use serde_json::Value;

use super::{GitHubActions, MutationControl, StewardLedger, record_audit};
use crate::merge_queue_control::{
    ARM_ACTION, UncertainReconciliation, UncertainVerdict, reconcile_uncertain_admissions,
};

/// `blocked_on` token for a refusal that names a pending code-owner review.
pub(super) const CODE_OWNER_REVIEW: &str = "code_owner_review";
/// `blocked_on` prefix for any other definitive enqueue refusal.
pub(super) const ENQUEUE_REFUSED_PREFIX: &str = "enqueue_refused:";

/// GraphQL error types that are GitHub declining the request itself.
const DEFINITIVE_GRAPHQL_TYPES: &[&str] = &["UNPROCESSABLE", "FORBIDDEN", "NOT_FOUND"];

/// GitHub's message when `text` (a mutation response, or a `gh` failure that
/// embeds one) is a definitive refusal; `None` when it is anything else.
#[must_use]
pub(super) fn definitive_refusal(text: &str) -> Option<String> {
    text.match_indices('{').find_map(|(start, _)| {
        let value = serde_json::Deserializer::from_str(&text[start..])
            .into_iter::<Value>()
            .next()?
            .ok()?;
        refusal_in(&value)
    })
}

fn refusal_in(value: &Value) -> Option<String> {
    let errors = value.get("errors")?.as_array()?;
    // A payload alongside the errors is a partial success, not a refusal.
    let payload_present = value
        .get("data")
        .and_then(Value::as_object)
        .is_some_and(|data| data.values().any(|field| !field.is_null()));
    if payload_present {
        return None;
    }
    let messages = errors
        .iter()
        .filter(|error| {
            error
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| DEFINITIVE_GRAPHQL_TYPES.contains(&kind))
        })
        .filter_map(|error| error.get("message").and_then(Value::as_str))
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>();
    (!messages.is_empty()).then(|| messages.join("; "))
}

/// The `blocked_on` token for a definitive refusal carrying `message`.
#[must_use]
pub(super) fn refusal_token(message: &str) -> String {
    if message.to_ascii_lowercase().contains("code owner review") {
        return CODE_OWNER_REVIEW.to_owned();
    }
    // The steward joins mutation tokens with commas, so the message must not
    // carry one.
    let compact = message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(',', ";");
    let compact = compact.trim_end_matches('.');
    let compact: String = compact.chars().take(160).collect();
    format!("{ENQUEUE_REFUSED_PREFIX}{compact}")
}

/// Whether one mutation-report part is a definitive enqueue refusal.
#[must_use]
pub(super) fn is_refusal_token(part: &str) -> bool {
    part == CODE_OWNER_REVIEW || part.starts_with(ENQUEUE_REFUSED_PREFIX)
}

/// Settle the uncertain admissions recorded for this repository and base.
///
/// Each settlement is written to the mutations ledger by
/// [`reconcile_uncertain_admissions`] and noted in the steward ledger. An
/// error here is reported, never fatal: an entry left uncertain blocks only
/// its own pull request.
pub(super) fn reconcile(
    actions: &GitHubActions,
    control: &MutationControl,
    repo: &str,
    base: &str,
    ledger: &mut StewardLedger,
) -> Result<Vec<UncertainReconciliation>, String> {
    let results = reconcile_uncertain_admissions(
        &control.state_dir,
        &control.global_dir,
        repo,
        base,
        |entry| judge_entry(actions, repo, entry),
    )?;
    for result in results
        .iter()
        .filter(|result| result.outcome != "uncertain")
    {
        record_audit(
            ledger,
            repo,
            &format!(
                "pr:{}:{}",
                result.pr.unwrap_or_default(),
                result.head.as_deref().unwrap_or_default()
            ),
            &format!(
                "uncertain_mutation_{}:{}",
                result.outcome, result.correlation_id
            ),
        );
    }
    Ok(results)
}

fn judge_entry(actions: &GitHubActions, repo: &str, entry: &Value) -> UncertainVerdict {
    let (Some(pr), Some(head)) = (
        entry.get("pr").and_then(Value::as_u64),
        entry.get("head").and_then(Value::as_str),
    ) else {
        return UncertainVerdict::Unresolved(
            "the uncertain mutation records no pull request or head".to_owned(),
        );
    };
    let action = entry
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match super::native_arm::read_queue_state(actions, repo, pr) {
        Ok(queue) => judge(&queue, head, action),
        Err(detail) => {
            UncertainVerdict::Unresolved(format!("PR #{pr} merge-queue state unreadable: {detail}"))
        }
    }
}

/// The verdict a `PR_QUEUE_STATE_QUERY` response gives on one uncertain
/// admission of `head` by `action`.
#[must_use]
pub(super) fn judge(queue: &Value, head: &str, action: &str) -> UncertainVerdict {
    if queue
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        return UncertainVerdict::Unresolved(
            "the merge-queue state read returned errors".to_owned(),
        );
    }
    let Some(pr) = queue
        .pointer("/data/repository/pullRequest")
        .filter(|pr| pr.is_object())
    else {
        return UncertainVerdict::Unresolved(
            "the merge-queue state read returned no pull request".to_owned(),
        );
    };
    let state = pr.get("state").and_then(Value::as_str).unwrap_or_default();
    if state.eq_ignore_ascii_case("MERGED") {
        return UncertainVerdict::Accepted("GitHub shows the pull request merged".to_owned());
    }
    let current_head = pr.get("headRefOid").and_then(Value::as_str);
    let at_recorded_head = current_head.is_some_and(|current| current.eq_ignore_ascii_case(head));
    let queued = pr
        .get("mergeQueueEntry")
        .is_some_and(|entry| !entry.is_null())
        || pr.get("isInMergeQueue").and_then(Value::as_bool) == Some(true);
    let armed = pr
        .get("autoMergeRequest")
        .is_some_and(|request| !request.is_null());
    match (queued, armed) {
        (true, _) if at_recorded_head => UncertainVerdict::Accepted(format!(
            "GitHub shows a merge-queue entry for the recorded head {}",
            short(head)
        )),
        (true, _) => UncertainVerdict::Unresolved(
            "the pull request is queued, but at a head other than the recorded one".to_owned(),
        ),
        (false, true) if action == ARM_ACTION && at_recorded_head => {
            UncertainVerdict::Accepted(format!(
                "GitHub shows native auto-merge armed on the recorded head {}",
                short(head)
            ))
        }
        (false, true) => UncertainVerdict::Unresolved(
            "an auto-merge request is armed but no queue entry exists yet".to_owned(),
        ),
        (false, false) => UncertainVerdict::Rejected(
            "GitHub shows no merge-queue entry, no auto-merge request, and no merge".to_owned(),
        ),
    }
}

fn short(head: &str) -> &str {
    head.get(..12).unwrap_or(head)
}

#[cfg(test)]
mod tests;
