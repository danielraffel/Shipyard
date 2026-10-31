//! Name why every open pull request is not merging, and since when.
//!
//! The steward's decision and mutation say what it did; they do not say, in
//! one token, what the pull request is waiting on. `waiting_head_approval`
//! alone covered a head nobody reviewed, a held head, a head whose approval
//! the gate excludes (an `unattributed` stamp, a signer no list counts) and a
//! head an owner must decide. Each report row therefore carries `blocked_on`,
//! one stable token, and `since`, when that state began, both derived from the
//! reads the pass already made. Only a state no read dated (a draft, a merge
//! conflict, a check that never ran) costs one more read: the head commit's
//! time, or the time a skip label was added.
//!
//! The token vocabulary is documented in `docs/automated-draining.md`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{
    ApprovalBlockers, GitHubActions, ObservedPr, PrReport, RepoObservation, StewardDecision,
    StewardLedger, StewardPolicy, gh_json,
};
use crate::head_approval::ApprovalBlocker;
use crate::merge_steward::{StewardPullRequest, selected_required_check};

/// The label an owner puts on a pull request only they can decide.
pub(super) const OWNER_DECISION_LABEL: &str = "owner-decision";
/// The label on a pull request another one replaced.
pub(super) const SUPERSEDED_LABEL: &str = "superseded";
/// The prefix of a `PARKED/<platform>` label.
pub(super) const PARKED_LABEL_PREFIX: &str = "parked/";

/// Tokens that are not blocked: the summary leaves them out of its oldest
/// list.
const NOT_BLOCKED: [&str; 4] = ["admitted", "queued", "ready", "draft"];

/// How many blocked pull requests the summary lists, oldest first.
const OLDEST_LISTED: usize = 3;

/// Why one pull request is not merging, and since when.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(super) struct Stall {
    /// One stable machine token (see the module documentation).
    pub blocked_on: String,
    /// UTC time the state began, best effort; `null` when nothing dates it.
    pub since: Option<String>,
    /// Which GitHub timestamp `since` came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_source: Option<&'static str>,
    /// Contexts, reason text or other detail behind the token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_detail: Option<String>,
    /// The HOLD behind a `held` token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_by: Option<HeldBy>,
}

/// The HOLD behind a `held` token.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct HeldBy {
    /// The HOLD's `reviewer:` signer, else its login.
    pub holder: String,
    /// `comment` or `review`.
    pub record: &'static str,
    /// The comment or review id.
    pub id: Option<u64>,
}

/// Per-repository roll-up of the `blocked_on` tokens in one pass.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(super) struct BlockedSummary {
    /// Pull requests per token.
    pub counts: BTreeMap<String, usize>,
    /// The oldest blocked pull requests, oldest first.
    pub oldest: Vec<OldestBlocked>,
}

/// One entry in [`BlockedSummary::oldest`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct OldestBlocked {
    /// Pull request number.
    pub number: u64,
    /// Its token.
    pub blocked_on: String,
    /// Minutes since `since`.
    pub age_minutes: i64,
}

/// Where `since` must still come from once classification is done.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SinceHint {
    /// `since` is already set (or nothing can date it).
    Known,
    /// The head commit's committer date: a best-effort stand-in for the push
    /// time, from the author's clock, so it can predate the push and the
    /// state.
    HeadPushed,
    /// The time this label was last added.
    Labeled(String),
}

/// The approval gate's verdict for one exact head, as this pass read it.
#[derive(Clone, Copy, Debug)]
pub(super) enum GateRead<'a> {
    /// Not read this pass, or the read failed.
    Unread,
    /// The gate admits the head.
    Admits,
    /// The gate refuses the head, for this reason.
    Refused(&'a ApprovalBlocker),
}

/// One exact-head gate read this pass made: the head it judged, why the gate
/// refuses it (`None` admits), and whether the merge queue already ejected
/// that same head, read from the same queue-state answer the admission path
/// judges ([`crate::pr_queue_state::internal_enqueue_verdict`]).
#[derive(Clone, Debug)]
pub(super) struct GateRecord {
    /// The head the gate judged.
    pub head: String,
    /// Why the gate refuses the head; `None` when it admits it.
    pub blocker: Option<ApprovalBlocker>,
    /// The queue removed this exact head and no new head followed.
    pub ejection: Option<SameHeadEjection>,
}

impl GateRecord {
    /// Record one gate read of `head` with the queue state it read.
    #[must_use]
    pub fn new(head: &str, blocker: Option<ApprovalBlocker>, queue: &serde_json::Value) -> Self {
        let report = crate::pr_queue_state::explain_pr_queue_state(queue);
        let ejection = match crate::pr_queue_state::internal_enqueue_verdict(&report, head) {
            crate::pr_queue_state::InternalEnqueueVerdict::RefuseEjectedSameHead { reason, at } => {
                Some(SameHeadEjection { reason, at })
            }
            _ => None,
        };
        Self {
            head: head.to_owned(),
            blocker,
            ejection,
        }
    }
}

/// The merge queue removed this exact head and will not take it back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SameHeadEjection {
    /// GitHub's removal reason.
    pub reason: String,
    /// When the removal happened.
    pub at: Option<String>,
}

/// What the pass knows about one row.
pub(super) struct RowFacts<'a> {
    /// The pull request as observed.
    pub pr: &'a StewardPullRequest,
    /// The steward's decision.
    pub decision: &'a StewardDecision,
    /// The mutation outcome, comma-joined.
    pub mutation: Option<&'a str>,
    /// The row's error.
    pub error: Option<&'a str>,
    /// The approval gate's verdict for this exact head.
    pub approval: GateRead<'a>,
    /// When the merge queue took this head, when it is queued.
    pub enqueued_at: Option<&'a str>,
    /// The queue's same-head ejection, when the gate read saw one.
    pub ejection: Option<&'a SameHeadEjection>,
}

/// Classify one row. Pure: `since` may still need a [`SinceHint`] read.
///
/// A HOLD stays visible in `held_by` even when a label or another state
/// outranks it in `blocked_on`.
#[must_use]
pub(super) fn classify(facts: &RowFacts<'_>, policy: &StewardPolicy) -> (Stall, SinceHint) {
    let (mut stall, hint) = classify_token(facts, policy);
    if stall.held_by.is_none()
        && let GateRead::Refused(ApprovalBlocker::Held {
            holder, record, id, ..
        }) = facts.approval
    {
        stall.held_by = Some(HeldBy {
            holder: holder.clone(),
            record,
            id: *id,
        });
    }
    (stall, hint)
}

fn classify_token(facts: &RowFacts<'_>, policy: &StewardPolicy) -> (Stall, SinceHint) {
    let pr = facts.pr;
    // What the steward did this pass outranks every label: a labelled PR that
    // was nonetheless admitted must read `admitted`, never its label.
    if admitted(facts.mutation) {
        return (
            Stall {
                blocked_on: "admitted".to_owned(),
                ..Stall::default()
            },
            SinceHint::Known,
        );
    }
    if let StewardDecision::Queued { position } = facts.decision {
        return dated(
            "queued".to_owned(),
            facts.enqueued_at.map(str::to_owned),
            "merge_queue_enqueued",
            Some(format!("position {position}")),
        );
    }
    if pr.draft || matches!(facts.decision, StewardDecision::Draft) {
        return undated("draft".to_owned(), None, SinceHint::HeadPushed);
    }
    if let Some(label) = find_label(pr, |label| label.eq_ignore_ascii_case(OWNER_DECISION_LABEL)) {
        return undated("owner_decision".to_owned(), None, SinceHint::Labeled(label));
    }
    if let Some(label) = find_label(pr, |label| {
        label.eq_ignore_ascii_case(SUPERSEDED_LABEL)
            || label.to_ascii_lowercase().starts_with(PARKED_LABEL_PREFIX)
    }) {
        return undated(
            format!("excluded_label:{label}"),
            None,
            SinceHint::Labeled(label),
        );
    }
    match facts.decision {
        // Both answered above; kept total without a panic.
        StewardDecision::Queued { .. } => undated("queued".to_owned(), None, SinceHint::Known),
        StewardDecision::Draft => undated("draft".to_owned(), None, SinceHint::HeadPushed),
        StewardDecision::OptedOut => undated(
            format!("excluded_label:{}", policy.opt_out_label),
            None,
            SinceHint::Labeled(policy.opt_out_label.clone()),
        ),
        StewardDecision::ProvenanceBlocked { labels } => undated(
            format!("provenance_blocked:{}", labels.join(",")),
            None,
            labels.first().map_or(SinceHint::HeadPushed, |label| {
                SinceHint::Labeled(label.clone())
            }),
        ),
        StewardDecision::Unmanaged => undated("unmanaged".to_owned(), None, SinceHint::HeadPushed),
        StewardDecision::HandoffMissing => {
            undated("handoff_missing".to_owned(), None, SinceHint::HeadPushed)
        }
        StewardDecision::InvalidHead => undated("invalid_head".to_owned(), None, SinceHint::Known),
        StewardDecision::NeedsUpdate { merge_state } => undated(
            format!("merge_state:{}", merge_state.to_ascii_uppercase()),
            None,
            SinceHint::HeadPushed,
        ),
        StewardDecision::WaitingRequired { contexts } => {
            approval_context_only(facts, policy, contexts)
                .unwrap_or_else(|| waiting_required(pr, policy, contexts))
        }
        StewardDecision::RequiredFailed { contexts } => {
            approval_context_only(facts, policy, contexts).unwrap_or_else(|| {
                let since = latest_check_time(pr, policy, contexts);
                dated(
                    format!("required_failed:{}", context_names(policy, contexts)),
                    since,
                    "check_completed",
                    None,
                )
            })
        }
        StewardDecision::RerunTransient { run_ids } => undated(
            "transient_rerun".to_owned(),
            Some(
                run_ids
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            SinceHint::HeadPushed,
        ),
        StewardDecision::DirectMergeRefused { .. } => undated(
            "direct_merge_refused".to_owned(),
            None,
            SinceHint::HeadPushed,
        ),
        StewardDecision::ArmMergeQueue => arm_candidate(facts, policy),
    }
}

/// A head every required check passed: what the admission attempt found.
fn arm_candidate(facts: &RowFacts<'_>, policy: &StewardPolicy) -> (Stall, SinceHint) {
    let green_since = || latest_check_time(facts.pr, policy, &required_labels(policy));
    let parts: Vec<&str> = facts
        .mutation
        .map(|mutation| mutation.split(',').collect())
        .unwrap_or_default();
    // The queue's own verdict on this exact head outranks the approval: a
    // dry run and an apply pass both read it from the gate's queue state.
    if let Some(ejection) = facts.ejection {
        return ejected(&ejection.reason, ejection.at.clone(), green_since());
    }
    if let Some(cause) = parts
        .iter()
        .find_map(|part| part.strip_prefix("skipped_ejected_same_head:"))
    {
        return ejected(cause, None, green_since());
    }
    if parts.contains(&"waiting_head_approval")
        || (facts.mutation.is_none() && facts.error.is_none())
    {
        return match facts.approval {
            GateRead::Refused(blocker) => approval_blocked(blocker, green_since()),
            GateRead::Admits => green("ready".to_owned(), green_since(), None),
            // The gate was never read for this head (or the read failed):
            // whether it is approved is unknown, never assumed.
            GateRead::Unread => green(
                "approval_unread".to_owned(),
                green_since(),
                facts.error.map(str::to_owned),
            ),
        };
    }
    if let Some(part) = parts
        .iter()
        .find(|part| **part == "waiting_full_test_tier" || **part == "waiting_enqueue_requirements")
    {
        return green(
            (*part).to_owned(),
            green_since(),
            facts.error.map(str::to_owned),
        );
    }
    if parts.iter().any(|part| part.starts_with("skipped_after")) {
        return green(
            "revalidating".to_owned(),
            green_since(),
            facts.mutation.map(str::to_owned),
        );
    }
    if let Some(error) = facts.error {
        return green(
            "steward_error".to_owned(),
            green_since(),
            Some(error.to_owned()),
        );
    }
    green(
        "admission_pending".to_owned(),
        green_since(),
        facts.mutation.map(str::to_owned),
    )
}

fn admitted(mutation: Option<&str>) -> bool {
    mutation.is_some_and(|mutation| {
        mutation
            .split(',')
            .any(|part| part.starts_with("enqueued") || part.starts_with("merged_exact_head"))
    })
}

fn ejected(cause: &str, at: Option<String>, green_since: Option<String>) -> (Stall, SinceHint) {
    let token = format!("ejected_same_head:{cause}");
    match at {
        Some(at) => dated(token, Some(at), "queue_removed", None),
        None => green(token, green_since, None),
    }
}

/// When every failing or pending required context is the approval context
/// (`review-verdict`), the approval is the blocker: name it. Without a gate
/// read for this head, keep the check token and say so.
fn approval_context_only(
    facts: &RowFacts<'_>,
    policy: &StewardPolicy,
    contexts: &[String],
) -> Option<(Stall, SinceHint)> {
    if !only_approval_contexts(policy, contexts) {
        return None;
    }
    let since = latest_check_time(facts.pr, policy, contexts);
    match facts.approval {
        GateRead::Refused(blocker) => Some(approval_blocked(blocker, since)),
        GateRead::Admits => None,
        GateRead::Unread => Some(dated(
            format!("required_failed:{}", context_names(policy, contexts)),
            since,
            "check_completed",
            Some(
                "only the approval context is not green; this pass did not read the approval \
                 gate for this head, so a dry run (without --apply) names the reason"
                    .to_owned(),
            ),
        )),
    }
}

/// Whether `contexts` (required-check labels) are all the approval context.
pub(super) fn only_approval_contexts(policy: &StewardPolicy, contexts: &[String]) -> bool {
    let approval = policy.approval_context.trim();
    !approval.is_empty()
        && !contexts.is_empty()
        && contexts
            .iter()
            .all(|label| context_name(policy, label).eq_ignore_ascii_case(approval))
}

/// The bare context name behind a required-check label.
fn context_name<'a>(policy: &'a StewardPolicy, label: &'a str) -> &'a str {
    policy
        .required_checks
        .iter()
        .find(|required| required.label() == label)
        .map_or_else(
            || {
                label
                    .strip_suffix(" (commit status)")
                    .unwrap_or(label)
                    .split(" (app_id=")
                    .next()
                    .unwrap_or(label)
            },
            |required| required.context.as_str(),
        )
}

fn context_names(policy: &StewardPolicy, labels: &[String]) -> String {
    let mut names: Vec<&str> = labels
        .iter()
        .map(|label| context_name(policy, label))
        .collect();
    names.dedup();
    names.join(",")
}

fn approval_blocked(blocker: &ApprovalBlocker, green_since: Option<String>) -> (Stall, SinceHint) {
    let (since, source) = match blocker.since() {
        // A stale-head approval dates the earlier head, not this state.
        Some(at) if !matches!(blocker, ApprovalBlocker::StaleHead { .. }) => {
            (Some(at.to_owned()), approval_since_source(blocker))
        }
        _ => (green_since, "checks_green"),
    };
    let detail = match blocker {
        ApprovalBlocker::Excluded { detail, id, .. } => {
            Some(id.map_or_else(|| detail.clone(), |id| format!("{detail} (record {id})")))
        }
        ApprovalBlocker::StaleHead { approved_head, .. } => {
            Some(format!("the newest approval names {approved_head}"))
        }
        ApprovalBlocker::SelfApproved { agent } => Some(format!("approved and armed by {agent}")),
        _ => None,
    };
    let held_by = match blocker {
        ApprovalBlocker::Held {
            holder, record, id, ..
        } => Some(HeldBy {
            holder: holder.clone(),
            record,
            id: *id,
        }),
        _ => None,
    };
    let hint = if since.is_some() {
        SinceHint::Known
    } else {
        SinceHint::HeadPushed
    };
    (
        Stall {
            blocked_on: blocker.token(),
            since_source: since.as_ref().map(|_| source),
            since,
            blocked_detail: detail,
            held_by,
        },
        hint,
    )
}

const fn approval_since_source(blocker: &ApprovalBlocker) -> &'static str {
    match blocker {
        ApprovalBlocker::Held { .. } => "hold_posted",
        ApprovalBlocker::EjectionCap { .. } => "queue_removed",
        _ => "approval_posted",
    }
}

/// Split missing contexts (never ran on this head) from pending ones.
fn waiting_required(
    pr: &StewardPullRequest,
    policy: &StewardPolicy,
    contexts: &[String],
) -> (Stall, SinceHint) {
    if contexts.len() == 1 && contexts[0] == "authoritative-required-check-policy" {
        return undated("required_policy_unknown".to_owned(), None, SinceHint::Known);
    }
    if let Some(state) = contexts
        .iter()
        .find_map(|context| context.strip_prefix("github-merge-state:CLEAN (current="))
    {
        return undated(
            format!("merge_state:{}", state.trim_end_matches(')')),
            None,
            SinceHint::HeadPushed,
        );
    }
    let absent: Vec<&crate::merge_steward::RequiredCheck> = policy
        .required_checks
        .iter()
        .filter(|required| contexts.contains(&required.label()))
        .filter(|required| selected_required_check(&pr.checks, required).is_none())
        .collect();
    // A context that has not appeared yet while runs from the same source are
    // still queued or running is pending (a job that `needs:` another has not
    // started); only an absent context with nothing in flight never ran.
    let in_flight = |required: &crate::merge_steward::RequiredCheck| {
        pr.checks.iter().any(|check| {
            !check.status.eq_ignore_ascii_case("COMPLETED")
                && required
                    .app_id
                    .is_none_or(|app_id| check.app_id == Some(app_id))
        })
    };
    let never_ran: Vec<&str> = absent
        .iter()
        .filter(|required| !in_flight(required))
        .map(|required| required.context.as_str())
        .collect();
    if !never_ran.is_empty() {
        return undated(
            format!("required_never_ran:{}", never_ran.join(",")),
            Some(context_names(policy, contexts)),
            SinceHint::HeadPushed,
        );
    }
    let since = policy
        .required_checks
        .iter()
        .filter(|required| contexts.contains(&required.label()))
        .filter_map(|required| selected_required_check(&pr.checks, required))
        .filter_map(|check| check.observed_at.clone())
        .min();
    let since = since.or_else(|| {
        pr.checks
            .iter()
            .filter(|check| !check.status.eq_ignore_ascii_case("COMPLETED"))
            .filter_map(|check| check.observed_at.clone())
            .min()
    });
    dated(
        "required_pending".to_owned(),
        since,
        "check_started",
        Some(context_names(policy, contexts)),
    )
}

/// The newest observed time among the checks satisfying `labels`.
fn latest_check_time(
    pr: &StewardPullRequest,
    policy: &StewardPolicy,
    labels: &[String],
) -> Option<String> {
    policy
        .required_checks
        .iter()
        .filter(|required| labels.contains(&required.label()))
        .filter_map(|required| selected_required_check(&pr.checks, required))
        .filter_map(|check| check.observed_at.clone())
        .max()
}

fn required_labels(policy: &StewardPolicy) -> Vec<String> {
    policy
        .required_checks
        .iter()
        .map(crate::merge_steward::RequiredCheck::label)
        .collect()
}

fn find_label(pr: &StewardPullRequest, predicate: impl Fn(&str) -> bool) -> Option<String> {
    pr.labels.iter().find(|label| predicate(label)).cloned()
}

fn dated(
    token: String,
    since: Option<String>,
    source: &'static str,
    detail: Option<String>,
) -> (Stall, SinceHint) {
    let hint = if since.is_some() {
        SinceHint::Known
    } else {
        SinceHint::HeadPushed
    };
    (
        Stall {
            blocked_on: token,
            since_source: since.as_ref().map(|_| source),
            since,
            blocked_detail: detail,
            held_by: None,
        },
        hint,
    )
}

fn green(token: String, since: Option<String>, detail: Option<String>) -> (Stall, SinceHint) {
    dated(token, since, "checks_green", detail)
}

fn undated(token: String, detail: Option<String>, hint: SinceHint) -> (Stall, SinceHint) {
    (
        Stall {
            blocked_on: token,
            blocked_detail: detail,
            ..Stall::default()
        },
        hint,
    )
}

/// Roll the rows up: counts by token and the oldest blocked rows.
#[must_use]
pub(super) fn summarize(reports: &[PrReport], now: DateTime<Utc>) -> BlockedSummary {
    let mut counts = BTreeMap::new();
    let mut aged = Vec::new();
    for report in reports {
        let token = &report.blocked.blocked_on;
        *counts.entry(token.clone()).or_insert(0) += 1;
        if NOT_BLOCKED.contains(&token.as_str()) {
            continue;
        }
        if let Some(since) = report
            .blocked
            .since
            .as_deref()
            .and_then(|since| DateTime::parse_from_rfc3339(since).ok())
        {
            aged.push(OldestBlocked {
                number: report.number,
                blocked_on: token.clone(),
                age_minutes: (now - since.with_timezone(&Utc)).num_minutes(),
            });
        }
    }
    aged.sort_by(|left, right| {
        right
            .age_minutes
            .cmp(&left.age_minutes)
            .then(left.number.cmp(&right.number))
    });
    aged.truncate(OLDEST_LISTED);
    BlockedSummary {
        counts,
        oldest: aged,
    }
}

/// Fill `blocked` on every row of one pass. A dry run reads the approval
/// gate itself for heads the pass would admit; an apply pass reuses the
/// verdicts its admission attempts read. A failed dating read leaves `since`
/// null and never fails the pass.
pub(super) fn annotate(
    actions: &GitHubActions,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    ledger: &StewardLedger,
    blockers: &ApprovalBlockers,
    read_gate: bool,
    reports: &mut [PrReport],
) {
    let mut dry_run_blockers = ApprovalBlockers::new();
    for report in reports.iter_mut() {
        let Some(pr) = observation
            .prs
            .iter()
            .find(|pr| pr.fact.number == report.number)
        else {
            "unclassified".clone_into(&mut report.blocked.blocked_on);
            continue;
        };
        if read_gate
            && dry_run_reads_gate(&report.decision, policy)
            && !blockers.contains_key(&report.number)
            && let Ok((_, queue, blocker)) =
                super::pr_mutations::read_head_gate_for_report(actions, observation, pr, ledger)
        {
            dry_run_blockers.insert(
                report.number,
                GateRecord::new(&pr.fact.head_sha, blocker, &queue),
            );
        }
        let record = blockers
            .get(&report.number)
            .or_else(|| dry_run_blockers.get(&report.number))
            .filter(|record| record.head.eq_ignore_ascii_case(&pr.fact.head_sha));
        let approval = record.map_or(GateRead::Unread, |record| {
            record
                .blocker
                .as_ref()
                .map_or(GateRead::Admits, GateRead::Refused)
        });
        let enqueued_at = observation.merge_group_enqueued_at.get(&report.number);
        let (mut blocked, hint) = classify(
            &RowFacts {
                pr: &pr.fact,
                decision: &report.decision,
                mutation: report.mutation.as_deref(),
                error: report.error.as_deref(),
                approval,
                enqueued_at: enqueued_at.map(String::as_str),
                ejection: record.and_then(|record| record.ejection.as_ref()),
            },
            policy,
        );
        fill_since(actions, &observation.repo, pr, &hint, &mut blocked);
        report.blocked = blocked;
    }
}

/// Which rows a dry run reads the approval gate for: the heads an apply pass
/// would try to admit, and the heads whose only red required context is the
/// approval context. An apply pass reads no more than its admission does.
fn dry_run_reads_gate(decision: &StewardDecision, policy: &StewardPolicy) -> bool {
    match decision {
        StewardDecision::ArmMergeQueue => true,
        StewardDecision::RequiredFailed { contexts }
        | StewardDecision::WaitingRequired { contexts } => only_approval_contexts(policy, contexts),
        _ => false,
    }
}

fn fill_since(
    actions: &GitHubActions,
    repo: &str,
    pr: &ObservedPr,
    hint: &SinceHint,
    blocked: &mut Stall,
) {
    if blocked.since.is_some() {
        return;
    }
    let (since, source) = match hint {
        SinceHint::Known => return,
        SinceHint::HeadPushed => (
            head_commit_time(actions, repo, &pr.fact.head_sha),
            "head_commit",
        ),
        SinceHint::Labeled(label) => (
            label_time(actions, repo, pr.fact.number, label),
            "label_added",
        ),
    };
    if since.is_some() {
        blocked.since = since;
        blocked.since_source = Some(source);
    }
}

fn head_commit_time(actions: &GitHubActions, repo: &str, head: &str) -> Option<String> {
    if !crate::merge_steward::is_full_sha(head) {
        return None;
    }
    gh_json(
        actions,
        &["api".to_owned(), format!("repos/{repo}/commits/{head}")],
        "head commit time",
    )
    .ok()?
    .pointer("/commit/committer/date")
    .and_then(serde_json::Value::as_str)
    .map(str::to_owned)
}

fn label_time(actions: &GitHubActions, repo: &str, number: u64, label: &str) -> Option<String> {
    let lines = actions
        .run_gh(&[
            "api".to_owned(),
            "--paginate".to_owned(),
            format!("repos/{repo}/issues/{number}/events?per_page=100"),
            "--jq".to_owned(),
            r#".[] | select(.event == "labeled") | [.label.name, .created_at] | @tsv"#.to_owned(),
        ])
        .ok()?;
    latest_label_time(&lines, label)
}

/// The newest `created_at` of a `labeled` event for `label` in tab-separated
/// `name<TAB>created_at` lines.
fn latest_label_time(lines: &str, label: &str) -> Option<String> {
    lines
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(name, _)| name.eq_ignore_ascii_case(label))
        .map(|(_, at)| at.trim().to_owned())
        .max()
}
