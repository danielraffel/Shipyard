//! Pause the merge queue while the base is poisoned, and restore it after.
//!
//! [`super::base_poison`] stops *new* admissions while a required context
//! fails every merge group, and fast-tracks a named fix. That leaves two
//! gaps this module closes:
//!
//! * **Entries already queued keep burning gates.** Each one forms a group
//!   that cannot pass, costs a full gate, and is ejected. With both
//!   `merge_group_hold = "on"` and `merge_group_pause = "on"` every queued
//!   entry the steward admitted that is not a candidate fix is removed from
//!   the queue ([`verdict`]), and its place is written to the
//!   steward ledger ([`PausedEntry`]). A pause is a steward removal, so it
//!   never counts toward the head's ejection cap. The pause defaults to
//!   `dry-run`, which only reports `would_pause`: replayed over a week of
//!   real merge groups the poison verdict was right far too rarely to remove
//!   work on it alone, so it stays in shadow until its record earns more.
//! * **Nothing put them back.** Once the poison clears (a merge group passes
//!   the context), every paused entry whose head is unchanged is admitted
//!   first, in its recorded queue order ([`restore_rank`]), through the same
//!   exact-head approval gate as any other admission. Nothing is enqueued
//!   here: the restore is an ordering, so a paused head that lost its
//!   approval or went red stays out.
//!
//! The pause only ever acts on the detector's deterministic verdict (a
//! context failing in consecutive merge groups that carried different pull
//! requests with none common to all), never on a single failure: one
//! pull request's own failure cannot pause the queue.
//!
//! A wrong pause is undone within one pass: set `merge_group_pause` (or the
//! hold) to `"off"` or `"dry-run"` in the base's `.shipyard/config.toml`.
//! The hold `off` restores at once; otherwise a merge group passing the
//! context restores every paused entry in order. A streak that merely went
//! stale, or an unread history, restores nothing, so a still-broken base is
//! not refilled.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::base_poison::RepoPoison;
use super::{
    MutationApplyContext, ObservedPr, PrReport, RepoObservation, StewardLedger,
    acquire_pr_mutation_guard,
};
use crate::base_health::PoisonHold;
use crate::merge_steward::{StewardDecision, StewardPullRequest};

/// The ledger audit action recorded for each pause.
pub(super) const PAUSE_AUDIT: &str = "dequeue_base_poisoned";
/// How long a paused entry waits to be restored before it is forgotten. A
/// poison that outlives this has bigger problems than queue order; the
/// pull request is then admitted like any other.
const RESTORE_TTL_HOURS: i64 = 48;

/// One queue entry the steward removed while the base was poisoned.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct PausedEntry {
    /// Pull-request number.
    pub(super) pr: u64,
    /// The head that was queued; a different head is not restored.
    pub(super) head: String,
    /// Its merge-queue position when paused (1 is the front).
    pub(super) position: u64,
    /// The poisoned contexts that caused the pause.
    pub(super) contexts: String,
    /// RFC 3339.
    pub(super) paused_at: String,
    /// Set once a pass reads the base clear (a merge group passed the
    /// context, or the operator turned the hold off); only then is the
    /// entry admitted first. A stale streak or an unread history is not a
    /// pass and restores nothing.
    #[serde(default)]
    pub(super) restoring: bool,
}

/// The ledger key for one repository and base.
pub(super) fn ledger_key(repo: &str, base: &str) -> String {
    format!("{}@{base}", repo.to_ascii_lowercase())
}

/// What the pause does with one open pull request this pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Verdict {
    /// Not queued, not poisoned, the chosen fix, or the hold is off.
    Keep,
    /// Remove it from the queue and record it for restore.
    Pause {
        /// Comma-joined poisoned contexts.
        contexts: String,
        /// Its queue position.
        position: u64,
    },
    /// Hold is `dry-run`: report what the pause would do.
    WouldPause {
        /// Comma-joined poisoned contexts.
        contexts: String,
    },
}

/// Decide one pull request's pause from the pass's merge-group reading.
/// Only an entry the steward itself admitted (`Queued`) is ever paused: an
/// opted-out or unowned entry would not be restored by the steward's
/// admission, so it is left alone. No candidate fix is paused, named or
/// measured, so a fix already in the queue keeps its group.
pub(super) fn verdict(
    poison: Option<&RepoPoison>,
    pr: &StewardPullRequest,
    decision: &StewardDecision,
) -> Verdict {
    let (Some(poison), Some(position)) = (poison, pr.queue_position) else {
        return Verdict::Keep;
    };
    if !matches!(decision, StewardDecision::Queued { .. }) {
        return Verdict::Keep;
    }
    let contexts = poison.health.poisoned_contexts();
    if contexts.is_empty()
        || poison.fixes.contains_key(&pr.number)
        || poison.candidates.contains(&pr.number)
    {
        return Verdict::Keep;
    }
    let contexts = contexts.join(",");
    if poison.policy.pauses() {
        Verdict::Pause { contexts, position }
    } else if poison.policy.hold == PoisonHold::Off {
        Verdict::Keep
    } else {
        Verdict::WouldPause { contexts }
    }
}

/// Pause `pr` when [`verdict`] says so: remove it from the queue under the
/// per-PR mutation guard and record its place. `None` when the pause does
/// not apply to it, so the pass evaluates it as usual.
pub(super) fn pause_pr(
    context: Option<&MutationApplyContext<'_>>,
    apply: bool,
    observation: &RepoObservation,
    pr: &ObservedPr,
    decision: &StewardDecision,
    poison: Option<&RepoPoison>,
    ledger: &mut StewardLedger,
) -> Option<PrReport> {
    let Verdict::Pause { contexts, position } = verdict(poison, &pr.fact, decision) else {
        return None;
    };
    let head = pr.fact.head_sha.clone();
    let report = |mutation: Option<String>, error: Option<String>| PrReport {
        number: pr.fact.number,
        head_sha: head.clone(),
        decision: decision.clone(),
        mutation,
        error,
        blocked: super::blocked_on::Stall::default(),
    };
    if !apply {
        return Some(report(
            Some(format!("would_pause_base_poisoned:{head}")),
            None,
        ));
    }
    let Some(context) = context else {
        return Some(report(
            None,
            Some("base-poison pause requires mutation control".to_owned()),
        ));
    };
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        observation,
        pr,
        "runner steward pause a queue entry while the base is poisoned",
    ) {
        Ok(guard) => guard,
        Err(error) => return Some(report(None, Some(error))),
    };
    let (mutation, error, outcome) = match super::unapproved_admission::dequeue_under_guard(
        context,
        pr,
        &head,
        ledger,
        PAUSE_AUDIT,
    ) {
        Ok(()) => {
            record(ledger, observation, pr, position, &contexts, Utc::now());
            (
                Some(format!("paused_base_poisoned:{head}")),
                None,
                "base_poison_paused",
            )
        }
        Err(error) => (
            None,
            Some(format!(
                "{error} (pausing because `{contexts}` is poisoned)"
            )),
            "base_poison_pause_failed",
        ),
    };
    let error = match (guard.finish(outcome), error) {
        (Ok(()), error) => error,
        (Err(audit), None) => Some(format!("mutation audit failed: {audit}")),
        (Err(audit), Some(error)) => Some(format!("{error}; mutation audit also failed: {audit}")),
    };
    Some(report(mutation, error))
}

/// Record that `pr` at `head` was paused from `position`.
pub(super) fn record(
    ledger: &mut StewardLedger,
    observation: &RepoObservation,
    pr: &ObservedPr,
    position: u64,
    contexts: &str,
    now: DateTime<Utc>,
) {
    let entries = ledger
        .poison_paused
        .entry(ledger_key(&observation.repo, &observation.base))
        .or_default();
    entries.retain(|entry| entry.pr != pr.fact.number);
    entries.push(PausedEntry {
        pr: pr.fact.number,
        head: pr.fact.head_sha.clone(),
        position,
        contexts: contexts.to_owned(),
        paused_at: now.to_rfc3339(),
        restoring: false,
    });
    entries.sort_by_key(|entry| (entry.position, entry.pr));
}

/// Where `number` goes in this pass's admission order: its rank among the
/// entries still waiting to be restored, or `None` when it waits for nothing.
pub(super) fn restore_rank(
    ledger: &StewardLedger,
    repo: &str,
    base: &str,
    number: u64,
) -> Option<usize> {
    ledger
        .poison_paused
        .get(&ledger_key(repo, base))?
        .iter()
        .filter(|entry| entry.restoring)
        .position(|entry| entry.pr == number)
}

/// Whether this pass's reading lets an entry paused for `contexts` come
/// back: the hold is off (the operator's undo), or a merge group passed every
/// one of those contexts. A stale streak, an empty sample, a shared-PR note
/// and an unread history pass nothing, so they keep it waiting.
fn restorable(poison: Option<&RepoPoison>, contexts: &str) -> bool {
    use crate::base_health::merge_group::MergeGroupHealth;
    let Some(poison) = poison else {
        return true;
    };
    if poison.policy.hold == PoisonHold::Off {
        return true;
    }
    match &poison.health {
        MergeGroupHealth::Clear { passed, .. } => contexts
            .split(',')
            .all(|context| passed.iter().any(|done| done == context)),
        MergeGroupHealth::Poisoned { .. } | MergeGroupHealth::Unknown { .. } => false,
    }
}

/// A paused pull request still waiting for a passing group is not admitted
/// by the normal path either: a streak that went stale reads as clear, and
/// admitting it there would refill a base that is still broken. Returns the
/// report for such a pull request, `None` for every other.
pub(super) fn waiting_report(
    ledger: &StewardLedger,
    observation: &RepoObservation,
    pr: &ObservedPr,
    decision: &StewardDecision,
) -> Option<PrReport> {
    if pr.fact.queue_position.is_some() {
        return None;
    }
    let entry = ledger
        .poison_paused
        .get(&ledger_key(&observation.repo, &observation.base))?
        .iter()
        .find(|entry| entry.pr == pr.fact.number && !entry.restoring)?;
    if !entry.head.eq_ignore_ascii_case(&pr.fact.head_sha) {
        return None;
    }
    Some(PrReport {
        number: pr.fact.number,
        head_sha: pr.fact.head_sha.clone(),
        decision: decision.clone(),
        mutation: Some(format!("paused_awaiting_pass:{}", entry.contexts)),
        error: None,
        blocked: super::blocked_on::Stall::default(),
    })
}

/// Why a paused entry stopped waiting, or `None` while it still waits.
fn settled(
    entry: &PausedEntry,
    prs: &[ObservedPr],
    poisoned: bool,
    now: DateTime<Utc>,
) -> Option<&'static str> {
    let expired = DateTime::parse_from_rfc3339(&entry.paused_at).map_or(true, |at| {
        now - at.with_timezone(&Utc) > Duration::hours(RESTORE_TTL_HOURS)
    });
    let Some(pr) = prs.iter().find(|pr| pr.fact.number == entry.pr) else {
        return Some("closed");
    };
    if !pr.fact.head_sha.eq_ignore_ascii_case(&entry.head) {
        return Some("head_moved");
    }
    if !poisoned && pr.fact.queue_position.is_some() {
        return Some("restored");
    }
    expired.then_some("expired")
}

/// Drop the paused entries that no longer wait, and report each one.
/// Entries keep waiting while the base is poisoned; a restored, closed,
/// moved or expired entry is forgotten.
pub(super) fn reconcile(
    ledger: &mut StewardLedger,
    observation: &RepoObservation,
    poison: Option<&RepoPoison>,
    now: DateTime<Utc>,
) -> Vec<String> {
    let key = ledger_key(&observation.repo, &observation.base);
    let Some(entries) = ledger.poison_paused.get_mut(&key) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    entries.retain_mut(|entry| {
        let restore = restorable(poison, &entry.contexts);
        let poisoned = !restore;
        if let Some(why) = settled(entry, &observation.prs, poisoned, now) {
            lines.push(format!("#{} {why}", entry.pr));
            return false;
        }
        entry.restoring = restore;
        lines.push(format!(
            "#{} paused from position {} ({}); {}",
            entry.pr,
            entry.position,
            entry.contexts,
            if poisoned {
                "restores when a merge group passes the context"
            } else {
                "restoring first, in queue order"
            }
        ));
        true
    });
    if entries.is_empty() {
        ledger.poison_paused.remove(&key);
    }
    lines
}

/// The pass report: what is paused, what would be, and how to undo it.
pub(super) fn report(
    observation: &RepoObservation,
    poison: Option<&RepoPoison>,
    lines: &[String],
) -> Value {
    let would = observation
        .prs
        .iter()
        .filter_map(|pr| {
            let decision = StewardDecision::Queued {
                position: pr.fact.queue_position?,
            };
            match verdict(poison, &pr.fact, &decision) {
                Verdict::WouldPause { contexts } => Some(format!(
                    "#{} would pause from position {} ({contexts})",
                    pr.fact.number,
                    pr.fact.queue_position.unwrap_or_default()
                )),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    json!({
        "paused": lines,
        "would_pause": would,
        "undo": "set merge_group_pause = \"dry-run\" (stop pausing) or merge_group_hold = \"off\" (stop pausing and restore now) under [base_health] in the base's .shipyard/config.toml",
    })
}

/// Open pull requests whose own head passes a poisoned context while at
/// least two other open heads fail it: the context is sensitive at PR
/// heads (the failure reproduces there), and this head is the one that no
/// longer fails. That is measured evidence of a fix, which
/// [`super::base_poison::choose_fix`] ranks below a label or the detector's
/// naming. The pass already read every head's checks, so this costs no
/// call. Only a check that completed after the poison's newest failure
/// counts, so a head that last ran before the base broke is not a fix.
pub(super) fn passing_head_fixes(
    prs: &[ObservedPr],
    poison: &crate::base_health::merge_group::MergeGroupHealth,
) -> BTreeMap<u64, Vec<String>> {
    let mut out: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for context in poison.poisoned() {
        let completed = |pr: &ObservedPr, want: fn(&str) -> bool| {
            pr.fact.checks.iter().any(|check| {
                check.name == context.context
                    && check.status.eq_ignore_ascii_case("COMPLETED")
                    && check.conclusion.as_deref().is_some_and(want)
            })
        };
        let failing = prs
            .iter()
            .filter(|pr| !pr.fact.draft && completed(pr, |c| c.eq_ignore_ascii_case("FAILURE")))
            .map(|pr| pr.fact.number)
            .collect::<Vec<_>>();
        if failing.len() < 2 {
            continue;
        }
        let since = context.latest_failure_at;
        for pr in prs.iter().filter(|pr| !pr.fact.draft) {
            let passed_after = pr.fact.checks.iter().any(|check| {
                check.name == context.context
                    && check.status.eq_ignore_ascii_case("COMPLETED")
                    && check
                        .conclusion
                        .as_deref()
                        .is_some_and(|c| c.eq_ignore_ascii_case("SUCCESS"))
                    && match (since, check.observed_at.as_deref()) {
                        (Some(since), Some(at)) => DateTime::parse_from_rfc3339(at)
                            .is_ok_and(|at| at.with_timezone(&Utc) > since),
                        _ => false,
                    }
            });
            if passed_after {
                out.entry(pr.fact.number).or_default().push(format!(
                    "head passes `{}` while {} other open heads fail it",
                    context.context,
                    failing.len()
                ));
            }
        }
    }
    out
}
