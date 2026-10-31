//! `shipyard review-verdict`: whether one pull request's head may land, by
//! the repository-side rule, as one JSON answer a merge-group check can run.
//!
//! It is [`crate::head_approval`]'s decision with the host taken out: review
//! is on for the repositories that require it by default (no host's opt-out
//! file or environment counts), the merge-only carry applies, and the rebase
//! carry applies unless the caller turns it off. The answer names the
//! approval or the carry that allowed the head, or why it is refused, and
//! every human approval the default rule ignored.

use serde::Serialize;
use serde_json::{Value, json};

use crate::head_approval::{self, Approval, ApprovalPolicy, HeadGate, RunGh};

/// The answer for one pull request's head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Verdict {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Pull request.
    pub pr: u64,
    /// The head judged.
    pub head: String,
    /// The pull request's head on GitHub now, when it differs from `head`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_head: Option<String>,
    /// Its base branch.
    pub base: String,
    /// `allowed` or `refused`.
    pub verdict: &'static str,
    /// Whether the repository requires an approval of the exact head.
    pub required: bool,
    /// One line saying why.
    pub reason: String,
    /// The approval that allowed the head, or carried to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
    /// Human approvals the default rule ignored (`human_reviewers` lists no
    /// such login).
    pub excluded_approvals: Vec<String>,
    /// Every reviewer note the decision ignored, as the receipts record them.
    pub audit_events: Vec<String>,
    /// What to do when refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Verdict {
    /// Whether the head may land.
    #[must_use]
    pub fn allowed(&self) -> bool {
        self.verdict == "allowed"
    }
}

/// Judge `head` (default: the pull request's current head) of `repo#pr`.
///
/// # Errors
///
/// When a needed fact cannot be read. A caller must treat that as refused.
pub fn review_verdict(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: Option<&str>,
    rebase_carry: bool,
) -> Result<Verdict, String> {
    let pull: Value = serde_json::from_str(
        run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/pulls/{pr}"),
            "--jq".to_owned(),
            "{head: .head.sha, base: .base.ref, author: .user.login}".to_owned(),
        ])?
        .trim(),
    )
    .map_err(|error| format!("PR #{pr} is unreadable: {error}"))?;
    let text = |key: &str| {
        pull.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let live = text("head").ok_or_else(|| format!("PR #{pr} carries no head"))?;
    let base = text("base").ok_or_else(|| format!("PR #{pr} carries no base branch"))?;
    let head = head.map_or_else(|| live.clone(), str::to_ascii_lowercase);
    let mut policy = head_approval::read_policy_repository_side(run_gh, repo, &base)?;
    policy.rebase_carry = rebase_carry;
    // A status is read-only: it judges a carry but never logs one.
    policy.dry_run = true;
    let (gate, events) = head_approval::evaluate_with_policy_for_author_with_events(
        run_gh,
        repo,
        pr,
        &head,
        &policy,
        &json!({}),
        text("author").as_deref(),
    )?;
    Ok(verdict_for(
        repo, pr, &head, &live, base, &policy, gate, events,
    ))
}

#[allow(clippy::too_many_arguments)]
fn verdict_for(
    repo: &str,
    pr: u64,
    head: &str,
    live: &str,
    base: String,
    policy: &ApprovalPolicy,
    gate: HeadGate,
    events: Vec<String>,
) -> Verdict {
    let allowed = gate.allows();
    let excluded_approvals = events
        .iter()
        .filter_map(|event| event.strip_prefix("excluded_human_approval:"))
        .map(str::to_owned)
        .collect();
    let holder = events
        .iter()
        .find_map(|event| event.strip_prefix("head_held:"));
    let fix = (!allowed).then(|| {
        if let Some(holder) = holder {
            return format!(
                "head {head} is {holder}, and a HOLD makes its head un-approvable: the author \
                 pushes a new head to https://github.com/{repo}/pull/{pr} that addresses the \
                 HOLD, and a reviewer reviews that new head"
            );
        }
        format!(
            "a reviewer agent reviews head {head} and posts, through `ghapp` (the fleet App), \
             a comment ending `verdict: APPROVE head:{head}` on \
             https://github.com/{repo}/pull/{pr}; a human approval counts only for a login \
             the base lists in `[auto_merge] human_reviewers`"
        )
    });
    Verdict {
        repo: repo.to_owned(),
        pr,
        head: head.to_owned(),
        live_head: (!live.eq_ignore_ascii_case(head)).then(|| live.to_owned()),
        base,
        verdict: if allowed { "allowed" } else { "refused" },
        required: policy.required,
        reason: gate.explain(),
        approval: match gate {
            HeadGate::Approved { approval } => Some(approval),
            _ => None,
        },
        excluded_approvals,
        audit_events: events,
        fix,
    }
}

#[cfg(test)]
mod tests;
