//! Arm GitHub-native auto-merge as soon as `ship` knows the pull request.
//!
//! ## Why here
//!
//! [`super::resolve_pr_context`] is the one place that has a pull-request
//! number for *every* route into `ship`: `shipyard pr` (which creates one),
//! a bare `shipyard ship` (which finds or creates one), and
//! `shipyard ship --pr <n>` (which adopts an existing one). Arming from that
//! single chokepoint means no route can be added later that quietly skips it.
//!
//! `ship --pr` needs this as much as the create path does: it queues a
//! validation without arming anything, so a pull request that was ejected and
//! then re-shipped came back `EJECTED` with `auto_merge_request: null` and sat
//! there.
//!
//! ## Why it never fails the ship
//!
//! Arming is a durability backstop, not the ship's purpose. A refusal is
//! usually the `ghapp` arm guard agreeing that there is nothing to do — the
//! pull request is already queued, already armed, or was ejected on this exact
//! head. Those are normal outcomes, so they are reported and stepped over.
//! Shipyard must never set `GHAPP_ALLOW_QUEUE_REARM` to push past one.
//!
//! The mutation therefore goes through the ordinary `run_gh` path rather than
//! [`crate::cloud::GitHubActions::run_gh_internal_queue_mutation`]: the
//! internal marker makes the guard step aside, and here we *want* the guard's
//! second opinion, because unlike Shipyard's validated enqueue this request is
//! not bound to a validated head.

use serde_json::Value;

use crate::auto_arm::{
    ArmVerdict, arm_mutation_args, decide_from_queue_state, is_arm_guard_refusal,
};
use crate::pr_queue_state::{PR_QUEUE_STATE_QUERY, explain_pr_queue_state};

/// What the arm attempt did, as one line for the ship transcript.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ArmOutcome {
    /// Whether GitHub accepted an arm request on this pass.
    pub(super) armed: bool,
    /// Human-readable line, already prefixed with its own marker.
    pub(super) line: String,
}

/// A `gh` transport. Returns stdout, or a message that includes stderr.
pub(super) type RunGh<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

/// Arm native auto-merge on `pr`, or explain why it was left alone.
///
/// Never returns an error: every failure mode is a reported line.
pub(super) fn arm_native_auto_merge(run_gh: RunGh<'_>, repo: &str, pr: u64) -> ArmOutcome {
    let facts = match read_pr_facts(run_gh, repo, pr) {
        Ok(facts) => facts,
        Err(detail) => {
            return skipped(format!(
                "⚠︎ Auto-merge not armed on #{pr}: its pull-request facts could not be read \
                 ({detail}). Check with `shipyard landing --pr {pr}`."
            ));
        }
    };
    let state = match read_queue_state(run_gh, repo, pr) {
        Ok(value) => explain_pr_queue_state(&value).state,
        Err(detail) => {
            return skipped(format!(
                "⚠︎ Auto-merge not armed on #{pr}: its merge-queue state could not be read \
                 ({detail}); refusing to arm blind. Check with `shipyard landing --pr {pr}`."
            ));
        }
    };

    match decide_from_queue_state(&state, facts.draft) {
        ArmVerdict::Skip(skip) => skipped(format!(
            "▸ Auto-merge left as it is on #{pr}: {}",
            skip.explain()
        )),
        ArmVerdict::Arm => match run_gh(&arm_mutation_args(&facts.node_id)) {
            Ok(raw) if arm_response_accepted(&raw) => ArmOutcome {
                armed: true,
                line: format!(
                    "▸ Auto-merge armed on #{pr} (merge method MERGE); GitHub enqueues it once \
                     its required checks pass."
                ),
            },
            Ok(raw) => skipped(format!(
                "⚠︎ Auto-merge not armed on #{pr}: GitHub accepted the request but returned no \
                 armed pull request ({}). Check with `shipyard landing --pr {pr}`.",
                first_graphql_error(&raw)
                    .unwrap_or_else(|| "no errors reported".to_owned())
            )),
            Err(detail) if is_arm_guard_refusal(&detail) => skipped(format!(
                "▸ Auto-merge left as it is on #{pr}: the queue-arm guard declined it, which is \
                 agreement that there is nothing to arm — {}",
                one_line(&detail)
            )),
            Err(detail) => skipped(format!(
                "⚠︎ Auto-merge not armed on #{pr}: {}. The ship continues; arm it later with \
                 `shipyard ship --pr {pr}` once the cause is cleared.",
                one_line(&detail)
            )),
        },
    }
}

fn skipped(line: String) -> ArmOutcome {
    ArmOutcome { armed: false, line }
}

/// The facts [`PR_QUEUE_STATE_QUERY`] deliberately does not carry.
///
/// `isDraft` and the node ID are read separately rather than added to that
/// query because `scripts/ghapp_queue_arm_guard.py` mirrors the query
/// document, and the two must stay in lockstep.
struct PrFacts {
    node_id: String,
    draft: bool,
}

fn read_pr_facts(run_gh: RunGh<'_>, repo: &str, pr: u64) -> Result<PrFacts, String> {
    let raw = run_gh(&[
        "pr".to_owned(),
        "view".to_owned(),
        pr.to_string(),
        "--repo".to_owned(),
        repo.to_owned(),
        "--json".to_owned(),
        "id,isDraft".to_owned(),
    ])?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|error| format!("malformed PR JSON: {error}"))?;
    let node_id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "PR view response carried no node ID".to_owned())?
        .to_owned();
    // An absent `isDraft` is unmeasured, not "not a draft": arming a draft is
    // refused by GitHub anyway, so failing closed costs nothing.
    let draft = value
        .get("isDraft")
        .and_then(Value::as_bool)
        .ok_or_else(|| "PR view response carried no isDraft".to_owned())?;
    Ok(PrFacts { node_id, draft })
}

fn read_queue_state(run_gh: RunGh<'_>, repo: &str, pr: u64) -> Result<Value, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("repo `{repo}` is not OWNER/REPO"))?;
    let raw = run_gh(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={PR_QUEUE_STATE_QUERY}"),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("number={pr}"),
    ])?;
    serde_json::from_str(&raw).map_err(|error| format!("malformed GraphQL JSON: {error}"))
}

/// Whether the mutation response proves a pull request came back armed.
///
/// A `200` carrying a GraphQL `errors` array is a failure; only the presence of
/// the mutation's own payload counts.
fn arm_response_accepted(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw).is_ok_and(|value| {
        value.get("errors").is_none()
            && value
                .pointer("/data/enablePullRequestAutoMerge/pullRequest")
                .is_some_and(|pull_request| !pull_request.is_null())
    })
}

fn first_graphql_error(raw: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw).ok().and_then(|value| {
        value
            .get("errors")?
            .as_array()?
            .first()?
            .get("message")?
            .as_str()
            .map(str::to_owned)
    })
}

/// Collapse a multi-line `gh` diagnostic so one arm result stays one line.
fn one_line(detail: &str) -> String {
    detail
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests;
