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
    ArmVerdict, arm_mutation_args, arm_mutation_args_at_head, arm_response_accepted,
    decide_from_queue_state_with_environment, first_graphql_error, is_arm_guard_refusal,
    is_auto_merge_disabled_refusal,
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
///
/// `environment_opt_in` is the repository's
/// [`crate::environment_requeue::CONFIG_KEY`]: when set, a head ejected once
/// for an environment failure is armed without a new push.
///
/// `override_by` is set when the operator passed `--arm`: the head-approval
/// gate ([`crate::head_approval`]) is skipped and the result names who
/// overrode it. Every other refusal still applies.
pub(super) fn arm_native_auto_merge(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    environment_opt_in: bool,
    override_by: Option<&str>,
) -> ArmOutcome {
    let facts = match read_pr_facts(run_gh, repo, pr) {
        Ok(facts) => facts,
        Err(detail) => {
            return skipped(format!(
                "⚠︎ Auto-merge not armed on #{pr}: its pull-request facts could not be read \
                 ({detail}). Check with `shipyard landing --pr {pr}`."
            ));
        }
    };
    let queue = match read_queue_state(run_gh, repo, pr) {
        Ok(value) => value,
        Err(detail) => {
            return skipped(format!(
                "⚠︎ Auto-merge not armed on #{pr}: its merge-queue state could not be read \
                 ({detail}); refusing to arm blind. Check with `shipyard landing --pr {pr}`."
            ));
        }
    };
    let report = explain_pr_queue_state(&queue);

    let environment = crate::environment_requeue::assess(run_gh, repo, &report, environment_opt_in);
    let verdict =
        decide_from_queue_state_with_environment(&report.state, facts.draft, environment.as_ref());
    let excuse = environment
        .as_ref()
        .is_some_and(crate::environment_requeue::EnvironmentRequeue::is_allowed_interruption);
    let override_note = match approval_note(
        run_gh,
        repo,
        pr,
        &verdict,
        &queue,
        &facts,
        override_by,
        excuse,
    ) {
        Ok(note) => note,
        Err(refusal) => return refusal,
    };
    match verdict {
        ArmVerdict::Skip(skip) => skipped(format!(
            "▸ Auto-merge left as it is on #{pr}: {}{}",
            skip.explain(),
            environment
                .as_ref()
                .filter(|verdict| !verdict.allowed)
                .map_or_else(String::new, |verdict| format!(
                    " (environment re-enqueue refused: {})",
                    verdict.reason
                ))
        )),
        ArmVerdict::Arm => match environment.as_ref().filter(|verdict| verdict.allowed) {
            Some(verdict) => arm_same_head(run_gh, pr, &facts, &queue, verdict, &override_note),
            None => arm_new(run_gh, pr, &facts, &override_note),
        },
    }
}

/// Arm `pr` at exactly the head the queue state names, after a classified
/// same-head re-enqueue.
fn arm_same_head(
    run_gh: RunGh<'_>,
    pr: u64,
    facts: &PrFacts,
    queue: &Value,
    verdict: &crate::environment_requeue::EnvironmentRequeue,
    override_note: &str,
) -> ArmOutcome {
    let Some(head) = queue
        .pointer("/data/repository/pullRequest/headRefOid")
        .and_then(Value::as_str)
        .filter(|head| !head.is_empty())
    else {
        return skipped(format!(
            "⚠︎ Auto-merge not armed on #{pr}: the merge-queue state carried no head to bind \
             the re-enqueue to."
        ));
    };
    match run_gh(&arm_mutation_args_at_head(&facts.node_id, head)) {
        Ok(raw) if arm_response_accepted(&raw) => ArmOutcome {
            armed: true,
            line: format!(
                "▸ Auto-merge armed on #{pr} (merge method MERGE) at exactly its current head, \
                 without a new push: a same-head re-enqueue ({}).{override_note}",
                verdict.reason
            ),
        },
        Ok(raw) => skipped(format!(
            "⚠︎ Auto-merge not armed on #{pr}: GitHub accepted the request but returned no armed \
             pull request ({}). Check with `shipyard landing --pr {pr}`.",
            first_graphql_error(&raw).unwrap_or_else(|| "no errors reported".to_owned())
        )),
        Err(detail) => skipped(format!(
            "▸ Auto-merge left as it is on #{pr}: the same-head re-enqueue was not accepted — {}",
            one_line(&detail)
        )),
    }
}

/// Arm `pr` on the ordinary path.
fn arm_new(run_gh: RunGh<'_>, pr: u64, facts: &PrFacts, override_note: &str) -> ArmOutcome {
    match run_gh(&arm_mutation_args(&facts.node_id)) {
        Ok(raw) if arm_response_accepted(&raw) => ArmOutcome {
            armed: true,
            line: format!(
                "▸ Auto-merge armed on #{pr} (merge method MERGE); GitHub enqueues it once \
                 its required checks pass.{override_note}"
            ),
        },
        Ok(raw) => skipped(format!(
            "⚠︎ Auto-merge not armed on #{pr}: GitHub accepted the request but returned no \
             armed pull request ({}). Check with `shipyard landing --pr {pr}`.",
            first_graphql_error(&raw).unwrap_or_else(|| "no errors reported".to_owned())
        )),
        Err(detail) if is_auto_merge_disabled_refusal(&detail) => skipped(format!(
            "▸ Auto-merge left as it is on #{pr}: this repository does not allow native \
             auto-merge, so there is nothing to arm."
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
    }
}

/// The note an arm result carries about head approval, or the refusal that
/// replaces it. Only an [`ArmVerdict::Arm`] consults the gate.
#[allow(clippy::too_many_arguments)]
fn approval_note(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    verdict: &ArmVerdict,
    queue: &Value,
    facts: &PrFacts,
    override_by: Option<&str>,
    excuse_latest_ejection: bool,
) -> Result<String, ArmOutcome> {
    match (verdict, override_by) {
        (ArmVerdict::Skip(_), _) => Ok(String::new()),
        (ArmVerdict::Arm, Some(by)) => Ok(format!(
            " Armed by an explicit `--arm` from {by}, without checking for an approval of the \
             head."
        )),
        (ArmVerdict::Arm, None) => {
            match head_gate(run_gh, repo, pr, queue, facts, excuse_latest_ejection) {
                Ok(gate) if gate.allows() => Ok(String::new()),
                Ok(gate) => Err(skipped(format!(
                    "▸ Auto-merge not armed on #{pr}: {}.",
                    gate.explain()
                ))),
                Err(detail) => Err(skipped(format!(
                    "⚠︎ Auto-merge not armed on #{pr}: whether its head is approved could not \
                 be read ({detail}); refusing to arm blind."
                ))),
            }
        }
    }
}

/// The head-approval verdict for the head the queue state names.
fn head_gate(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    queue: &Value,
    facts: &PrFacts,
    excuse_latest_ejection: bool,
) -> Result<crate::head_approval::HeadGate, String> {
    let head = queue
        .pointer("/data/repository/pullRequest/headRefOid")
        .and_then(Value::as_str)
        .filter(|head| !head.is_empty())
        .ok_or_else(|| "the merge-queue state carried no head".to_owned())?;
    let base = facts
        .base
        .as_deref()
        .ok_or_else(|| "the pull request carried no base branch".to_owned())?;
    crate::head_approval::evaluate_excusing(
        run_gh,
        repo,
        pr,
        head,
        base,
        queue,
        excuse_latest_ejection,
    )
}

/// Act on what the invocation asked of auto-merge: nothing for `--no-arm`,
/// an approval-gated arm by default, and an override for `--arm` that names
/// [`operator_identity`] as who armed it.
pub(super) fn arm_for_request(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    environment_opt_in: bool,
    request: crate::app::cli::ArmRequest,
) -> Option<ArmOutcome> {
    use crate::app::cli::ArmRequest;
    let override_by = match request {
        ArmRequest::Off => return None,
        ArmRequest::Default => None,
        ArmRequest::Override => Some(operator_identity()),
    };
    Some(arm_native_auto_merge(
        run_gh,
        repo,
        pr,
        environment_opt_in,
        override_by.as_deref(),
    ))
}

/// Who is running this command, for the record an `--arm` override leaves.
///
/// The GitHub credential is shared by every agent on a host, so it names no
/// one; the operating-system user and host name at least name the machine and
/// account the override came from.
pub(super) fn operator_identity() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "an unknown user".to_owned());
    let host = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "an unknown host".to_owned());
    format!("{user}@{host}")
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
    /// Base branch, where the head-approval policy is read from.
    base: Option<String>,
}

fn read_pr_facts(run_gh: RunGh<'_>, repo: &str, pr: u64) -> Result<PrFacts, String> {
    let raw = run_gh(&[
        "pr".to_owned(),
        "view".to_owned(),
        pr.to_string(),
        "--repo".to_owned(),
        repo.to_owned(),
        "--json".to_owned(),
        "id,isDraft,baseRefName".to_owned(),
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
    let base = value
        .get("baseRefName")
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty())
        .map(str::to_owned);
    Ok(PrFacts {
        node_id,
        draft,
        base,
    })
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
