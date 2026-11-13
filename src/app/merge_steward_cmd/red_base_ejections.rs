//! Re-enqueue pull requests the red base ejected, once the base is green.
//!
//! While a context is confirmed red ([`super::base_poison`]), every merge
//! group fails and GitHub ejects its pull requests with `failed_checks`.
//! Those ejections count toward the head's ejection cap
//! ([`crate::head_approval::EJECTION_CAP`]), so a head the red base ejected
//! twice is never armed again, though it did nothing wrong.
//!
//! With `[base_health] reenqueue_red_base_ejections = "on"`:
//!
//! * **Record.** Each pass while a context is confirmed, every open pull
//!   request that rode one of its failing merge groups is recorded at its
//!   current head, with the episode's window (from [`LOOKBACK`] before the
//!   base went red, so the streak's own groups count).
//! * **Release.** The first fully read pass with no context confirmed that
//!   reads the episode over (the tip read and no longer confirmed, or a
//!   merge group passing the context) closes the window and marks the entry
//!   ready.
//! * **Excuse.** At admission ([`excused_queue`]), a ready entry removes from
//!   the head's queue timeline its `failed_checks` removals inside the
//!   window, but only when the head's latest removal is one of them: a head
//!   ejected again after the base went green is judged as usual. The head
//!   still passes every other gate (exact-head approval, green checks), and
//!   the normal enqueue path admits it. Nothing is enqueued here.
//! * **Once per head.** An entry is bound to the head it recorded. It is
//!   dropped once that head is queued again (reported `re-enqueued`), when
//!   the head moves or the pull request closes, or after [`TTL_HOURS`].
//!
//! `dry-run` records and reports `would re-enqueue` and excuses nothing.
//! Reads: none. The timeline is the one the admission gate already reads.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::base_poison::{RepoPoison, confirmed_red_since};
use super::{RepoObservation, StewardLedger};
use crate::base_health::PoisonHold;
use crate::base_health::merge_group::MergeGroupHealth;

/// How far before the base went red an ejection still counts: the streak's
/// own groups failed before the tip confirmed it.
pub(super) const LOOKBACK: Duration = Duration::hours(2);
/// How long an entry waits to be re-enqueued before it is forgotten.
const TTL_HOURS: i64 = 48;

/// One head the red base ejected.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Entry {
    /// Pull-request number.
    pub(super) pr: u64,
    /// The head recorded; another head is never excused.
    pub(super) head: String,
    /// The confirmed context.
    pub(super) context: String,
    /// Window start (RFC 3339).
    pub(super) from: String,
    /// Window end, once the episode is over (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) until: Option<String>,
    /// Whether the excuse applies (`on` and the episode over).
    #[serde(default)]
    pub(super) ready: bool,
    /// When it was recorded (RFC 3339), for the TTL.
    pub(super) recorded_at: String,
}

fn key(repo: &str, base: &str) -> String {
    format!("{}@{base}", repo.to_ascii_lowercase())
}

fn time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Whether this pass reads `context`'s episode as over: the tip was read
/// and the context is not confirmed, or a merge group passed it. An unread
/// tip or an unread history says nothing.
fn episode_over(poison: Option<&RepoPoison>, context: &str) -> bool {
    let Some(poison) = poison else {
        return false;
    };
    // Nothing is released on a partial pass, or while any context is still
    // confirmed: a head excused now would only be held again.
    if poison.partial().is_some() || !poison.confirmed_contexts().is_empty() {
        return false;
    }
    match &poison.health {
        MergeGroupHealth::Clear { passed, .. } => passed.iter().any(|done| done == context),
        MergeGroupHealth::Poisoned { .. } => poison.tip_green_for(context),
        MergeGroupHealth::Unknown { .. } => false,
    }
}

/// The pass step: record, release and settle. Returns report lines.
pub(super) fn reconcile(
    mode: PoisonHold,
    apply: bool,
    ledger: &mut StewardLedger,
    observation: &RepoObservation,
    poison: Option<&RepoPoison>,
    now: DateTime<Utc>,
) -> Vec<String> {
    let line = |text: String| format!("red_base_reenqueue: {text}");
    let key = key(&observation.repo, &observation.base);
    if mode == PoisonHold::Off {
        ledger.red_base_ejections.remove(&key);
        return Vec::new();
    }
    let mut lines = Vec::new();
    let entries = ledger.red_base_ejections.entry(key.clone()).or_default();
    // Record.
    if let Some(poison) = poison {
        let confirmed = confirmed_red_since(&poison.health, &poison.tip, &poison.red_everywhere);
        for poisoned in poison.health.poisoned() {
            // An unknown onset gives no window an ejection can be shown to
            // fall in, so nothing is recorded and nothing is excused.
            let Some(since) = confirmed
                .get(&poisoned.context)
                .and_then(|since| crate::base_health::fix_first::known_onset(*since))
            else {
                continue;
            };
            for number in poisoned.groups.iter().flat_map(|group| group.prs.iter()) {
                let Some(pr) = observation.prs.iter().find(|pr| pr.fact.number == *number) else {
                    continue;
                };
                // One window per (pull request, head, context): a head that
                // rode the failing groups of two contexts with different red
                // onsets keeps both windows, so an ejection in either is
                // excused.
                if pr.fact.queue_position.is_some()
                    || entries.iter().any(|entry| {
                        entry.pr == *number
                            && entry.head == pr.fact.head_sha
                            && entry.context == poisoned.context
                    })
                {
                    continue;
                }
                entries.push(Entry {
                    pr: *number,
                    head: pr.fact.head_sha.clone(),
                    context: poisoned.context.clone(),
                    from: (since - LOOKBACK).to_rfc3339(),
                    until: None,
                    ready: false,
                    recorded_at: now.to_rfc3339(),
                });
            }
        }
    }
    // Release and settle.
    entries.retain_mut(|entry| {
        let pr = observation
            .prs
            .iter()
            .find(|pr| pr.fact.number == entry.pr);
        let gone = match pr {
            None => Some("closed"),
            Some(pr) if !pr.fact.head_sha.eq_ignore_ascii_case(&entry.head) => Some("head moved"),
            Some(pr) if entry.ready && pr.fact.queue_position.is_some() => Some("re-enqueued"),
            _ => time(&entry.recorded_at)
                .is_none_or(|at| now - at > Duration::hours(TTL_HOURS))
                .then_some("expired"),
        };
        if let Some(why) = gone {
            lines.push(line(format!("#{} at {}: {why}", entry.pr, short(&entry.head))));
            return false;
        }
        if entry.until.is_none() && episode_over(poison, &entry.context) {
            entry.until = Some(now.to_rfc3339());
        }
        if entry.until.is_some() {
            if mode == PoisonHold::On && apply {
                entry.ready = true;
                lines.push(line(format!(
                    "#{} at {}: red-base ejections on `{}` excused from the ejection cap; admitted as usual",
                    entry.pr,
                    short(&entry.head),
                    entry.context
                )));
            } else {
                lines.push(line(format!(
                    "#{} at {}: would re-enqueue (ejected while `{}` was red)",
                    entry.pr,
                    short(&entry.head),
                    entry.context
                )));
            }
        }
        true
    });
    if entries.is_empty() {
        ledger.red_base_ejections.remove(&key);
    }
    lines
}

/// The pass step in either mode. An applying pass records and releases in
/// the steward ledger. A report pass runs the same reconcile on a scratch
/// copy of the entries at no more than `dry-run`, so it reports what the act
/// would do and persists nothing.
pub(super) fn reconcile_for_pass(
    mode: PoisonHold,
    apply: bool,
    ledger: &mut StewardLedger,
    observation: &RepoObservation,
    poison: Option<&RepoPoison>,
    now: DateTime<Utc>,
) -> Vec<String> {
    if apply {
        return reconcile(mode, true, ledger, observation, poison, now);
    }
    let mut scratch = StewardLedger {
        red_base_ejections: ledger.red_base_ejections.clone(),
        ..StewardLedger::default()
    };
    reconcile(
        mode.min_dry_run(),
        false,
        &mut scratch,
        observation,
        poison,
        now,
    )
}

fn short(head: &str) -> &str {
    head.get(..12).unwrap_or(head)
}

/// `queue` with the head's red-base `failed_checks` removals taken out, when
/// a ready entry covers this exact head and the head's latest removal is one
/// of them. Otherwise `queue` unchanged.
pub(super) fn excused_queue(
    ledger: &StewardLedger,
    repo: &str,
    base: &str,
    pr: u64,
    head: &str,
    queue: &Value,
) -> Value {
    let windows = ledger
        .red_base_ejections
        .get(&key(repo, base))
        .into_iter()
        .flatten()
        .filter(|entry| entry.ready && entry.pr == pr && entry.head.eq_ignore_ascii_case(head))
        .filter_map(|entry| Some((time(&entry.from)?, time(entry.until.as_deref()?)?)))
        .collect::<Vec<_>>();
    if windows.is_empty() {
        return queue.clone();
    }
    let red_base = |node: &Value| {
        node.get("reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.eq_ignore_ascii_case("failed_checks"))
            && node
                .get("createdAt")
                .and_then(Value::as_str)
                .and_then(time)
                .is_some_and(|at| {
                    windows
                        .iter()
                        .any(|(from, until)| at >= *from && at <= *until)
                })
    };
    let latest = queue
        .pointer("/data/repository/pullRequest/timelineItems/nodes")
        .and_then(Value::as_array)
        .and_then(|nodes| {
            nodes
                .iter()
                .rev()
                .find(|node| super::unapproved_admission::is_removal_of(node, head))
        });
    if !latest.is_some_and(red_base) {
        return queue.clone();
    }
    super::unapproved_admission::filter_removals(queue, head, |node| !red_base(node))
}

/// Every entry for a base, for tests.
#[cfg(test)]
pub(super) fn entries<'a>(ledger: &'a StewardLedger, repo: &str, base: &str) -> &'a [Entry] {
    ledger
        .red_base_ejections
        .get(&key(repo, base))
        .map_or(&[], Vec::as_slice)
}
