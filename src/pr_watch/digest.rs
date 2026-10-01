//! The hourly digest: at most one per interval, carrying only flags that have
//! held on the same head for the minimum age and have not been sent before.
//!
//! One line per pull request: its highest-severity flag, with a count of the
//! flags it has. A repeated test failure that other pull requests share
//! ("failing on main/pre-existing") is not an owner action, so it does not
//! appear per pull request; instead the digest carries at most one "shared
//! failure" line per test, re-announced no sooner than
//! [`SHARED_REANNOUNCE_HOURS`]. A flag routed comment-only (an ejection the
//! batch attributor pinned on a neighbour) never reaches the digest.
//!
//! Delivery is claim-then-send. The claim is persisted before the configured
//! command runs; a failed command rolls the claim back so the next pass
//! retries; a claim found at start (the process died after claiming) is
//! treated as delivered, so a lost state write never double-posts.
//!
//! The payload is the `shipyard.pr-watch.digest/v1` JSON contract, written to
//! the command's stdin. Every `flags[]` entry keeps the contract's fields;
//! `count`, `kinds` and the top-level `shared_failures` are additive. An
//! empty digest is never sent.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::flags::{DigestRoute, FlagKind};
use super::ledger::{DigestClaim, Ledger, LedgerEntry};

/// Digest schema identifier.
pub const DIGEST_SCHEMA: &str = "shipyard.pr-watch.digest/v1";
/// A shared-failure test is announced at most once per this many hours.
pub const SHARED_REANNOUNCE_HOURS: i64 = 24;

/// Digest timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigestPolicy {
    /// Minimum time between digests.
    pub interval: Duration,
    /// Minimum age of a flag on the same head.
    pub min_age: Duration,
}

impl Default for DigestPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::minutes(60),
            min_age: Duration::minutes(120),
        }
    }
}

/// The digest contract payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestPayload {
    /// [`DIGEST_SCHEMA`].
    pub schema: String,
    /// `OWNER/REPO`.
    pub repo: String,
    /// When it was built.
    pub generated_at: String,
    /// Selection window.
    pub window: DigestWindow,
    /// One entry per pull request.
    pub flags: Vec<DigestFlag>,
    /// At most one entry per test failing across pull requests.
    #[serde(default)]
    pub shared_failures: Vec<SharedFailure>,
}

impl DigestPayload {
    /// Lines a renderer would print: one per pull request plus one per
    /// shared failure.
    #[must_use]
    pub fn lines(&self) -> usize {
        self.flags.len() + self.shared_failures.len()
    }
}

/// `window` object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestWindow {
    /// Minimum flag age, minutes.
    pub min_age_minutes: i64,
}

/// One pull request's digest line: its highest-severity flag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestFlag {
    /// Pull request.
    pub pr: u64,
    /// Title.
    pub title: String,
    /// URL.
    pub url: String,
    /// Contract kind name of the highest-severity flag.
    pub kind: String,
    /// Its verdict.
    pub verdict: String,
    /// Its evidence line.
    pub evidence: String,
    /// Earliest episode start among the pull request's flags in this digest.
    pub first_seen_at: String,
    /// Age of that earliest episode, minutes.
    pub age_minutes: i64,
    /// Head the flags hold on.
    pub head_sha: String,
    /// Flags the pull request has in this digest.
    pub count: usize,
    /// Their kinds, highest severity first.
    pub kinds: Vec<String>,
    /// Hand-back owner observation, when the hand-back ran. `unowned` marks
    /// tier 2: the owner has not been live for the configured time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<DigestOwner>,
}

/// The owner of a digest line's pull request, as the hand-back last saw it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestOwner {
    /// `live`, `dead`, `unknown`, `unreachable`, or `none`.
    pub state: String,
    /// Not live for at least `unowned_after_hours`.
    pub unowned: bool,
    /// Agent, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Host as stamped, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Session id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Resume hint for a person (never run by Shipyard).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// Worktree path, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

fn digest_owner(ledger: &Ledger, pr: u64) -> Option<DigestOwner> {
    let record = ledger.handback.owners.get(&pr)?;
    let owner = record.owner.as_ref();
    Some(DigestOwner {
        state: record.state.clone(),
        unowned: record.unowned,
        agent: owner.map(|o| o.agent.clone()),
        host: owner.and_then(|o| o.host.clone()),
        session: owner.map(|o| o.session.clone()),
        resume: owner.and_then(|o| o.resume.clone()),
        path: owner.and_then(|o| o.path.clone()),
    })
}

/// One test failing across pull requests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedFailure {
    /// Required check.
    pub check: String,
    /// Failing test (or error signature).
    pub test: String,
    /// Pull requests seen failing it.
    pub prs: Vec<u64>,
    /// Always "likely main/cross-PR".
    pub verdict: String,
    /// The line to print.
    pub evidence: String,
    /// Earliest episode start behind it.
    pub first_seen_at: String,
}

fn stamp(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// What a digest at one instant would carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Per pull request: entry ids, highest severity first.
    pub per_pr: BTreeMap<u64, Vec<String>>,
    /// Per shared test key (`check|test`): contributing entry ids.
    pub shared: BTreeMap<String, Vec<String>>,
}

impl Selection {
    /// Every entry id the digest carries.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: BTreeSet<String> = BTreeSet::new();
        ids.extend(self.per_pr.values().flatten().cloned());
        ids.extend(self.shared.values().flatten().cloned());
        ids.into_iter().collect()
    }
}

fn shared_key(entry: &LedgerEntry, test: &str) -> String {
    format!("{}|{test}", entry.key)
}

/// What a digest at `now` would carry. `None` when the interval has not
/// elapsed or nothing qualifies.
#[must_use]
pub fn select(ledger: &Ledger, now: DateTime<Utc>, policy: DigestPolicy) -> Option<Selection> {
    if ledger
        .last_digest_at
        .is_some_and(|last| now - last < policy.interval)
    {
        return None;
    }
    let eligible = ledger.entries.iter().filter(|(_, entry)| {
        entry.addressed_at.is_none()
            && entry.digested_at.is_none()
            && now - entry.first_seen_at >= policy.min_age
    });
    let mut selection = Selection::default();
    let mut splits: Vec<(&String, &LedgerEntry)> = Vec::new();
    let reannounce = Duration::hours(SHARED_REANNOUNCE_HOURS);
    for (id, entry) in eligible {
        match entry.route {
            DigestRoute::CommentOnly => {}
            DigestRoute::Shared => {
                for test in &entry.shared_tests {
                    let key = shared_key(entry, test);
                    let recent = ledger
                        .shared_announced
                        .get(&key)
                        .is_some_and(|at| now - *at < reannounce);
                    if !recent {
                        selection.shared.entry(key).or_default().push(id.clone());
                    }
                }
            }
            DigestRoute::PerPr if entry.kind == FlagKind::SplitCandidate => {
                splits.push((id, entry));
            }
            DigestRoute::PerPr => selection
                .per_pr
                .entry(entry.pr)
                .or_default()
                .push(id.clone()),
        }
    }
    // The split advisory never travels alone: only beside another flag on the
    // same pull request in this digest.
    for (id, entry) in splits {
        if let Some(ids) = selection.per_pr.get_mut(&entry.pr) {
            ids.push(id.clone());
        }
    }
    for ids in selection.per_pr.values_mut() {
        ids.sort_by_key(|id| {
            std::cmp::Reverse(
                ledger
                    .entries
                    .get(id)
                    .map_or(0, |entry| entry.kind.severity()),
            )
        });
    }
    if selection.per_pr.is_empty() && selection.shared.is_empty() {
        return None;
    }
    Some(selection)
}

/// Build the payload for a selection.
#[must_use]
pub fn payload(
    ledger: &Ledger,
    selection: &Selection,
    now: DateTime<Utc>,
    policy: DigestPolicy,
) -> DigestPayload {
    let flags = selection
        .per_pr
        .iter()
        .filter_map(|(pr, ids)| {
            let entries: Vec<&LedgerEntry> =
                ids.iter().filter_map(|id| ledger.entries.get(id)).collect();
            let top = entries.first()?;
            let first_seen = entries.iter().map(|entry| entry.first_seen_at).min()?;
            Some(DigestFlag {
                pr: *pr,
                title: top.title.clone(),
                url: top.url.clone(),
                kind: top.kind.as_str().to_owned(),
                verdict: top.verdict.clone(),
                evidence: top.evidence.clone(),
                first_seen_at: stamp(first_seen),
                age_minutes: (now - first_seen).num_minutes(),
                head_sha: top.head_sha.clone(),
                count: entries.len(),
                kinds: entries
                    .iter()
                    .map(|entry| entry.kind.as_str().to_owned())
                    .collect(),
                owner: digest_owner(ledger, *pr),
            })
        })
        .collect();
    let shared_failures = selection
        .shared
        .iter()
        .filter_map(|(key, ids)| {
            let (check, test) = key.split_once('|')?;
            let entries: Vec<&LedgerEntry> =
                ids.iter().filter_map(|id| ledger.entries.get(id)).collect();
            let mut prs: BTreeSet<u64> = BTreeSet::new();
            for entry in &entries {
                prs.insert(entry.pr);
                prs.extend(entry.related_prs.iter().copied());
            }
            let first_seen = entries.iter().map(|entry| entry.first_seen_at).min()?;
            let names: Vec<String> = prs.iter().map(|pr| format!("#{pr}")).collect();
            Some(SharedFailure {
                check: check.to_owned(),
                test: test.to_owned(),
                prs: prs.into_iter().collect(),
                verdict: "likely main/cross-PR".to_owned(),
                evidence: format!(
                    "`{test}` (`{check}`) failing across {} — likely main/cross-PR",
                    names.join(", ")
                ),
                first_seen_at: stamp(first_seen),
            })
        })
        .collect();
    DigestPayload {
        schema: DIGEST_SCHEMA.to_owned(),
        repo: ledger.repo.clone(),
        generated_at: stamp(now),
        window: DigestWindow {
            min_age_minutes: policy.min_age.num_minutes(),
        },
        flags,
        shared_failures,
    }
}

/// What one digest attempt did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DigestOutcome {
    /// Nothing qualified, or the interval had not elapsed.
    Skipped,
    /// Built but not sent (dry run).
    WouldSend {
        /// Lines it would carry.
        lines: usize,
    },
    /// Delivered.
    Sent {
        /// Lines it carried.
        lines: usize,
    },
}

fn mark_sent(ledger: &mut Ledger, ids: &[String], shared: &[String], at: DateTime<Utc>) {
    for id in ids {
        if let Some(entry) = ledger.entries.get_mut(id) {
            entry.digested_at = Some(at);
        }
    }
    for key in shared {
        ledger.shared_announced.insert(key.clone(), at);
    }
    let horizon = at - Duration::hours(SHARED_REANNOUNCE_HOURS * 7);
    ledger
        .shared_announced
        .retain(|_, announced| *announced > horizon);
    ledger.last_digest_at = Some(at);
}

/// Settle a claim left by an interrupted attempt: treat it as delivered.
pub fn settle_stale_claim(ledger: &mut Ledger) -> bool {
    let Some(claim) = ledger.digest_claim.take() else {
        return false;
    };
    mark_sent(ledger, &claim.ids, &claim.shared, claim.claimed_at);
    true
}

/// One digest attempt, claim-then-send. `persist` writes the ledger; `send`
/// delivers the payload JSON. With `post == false` nothing is claimed or
/// sent and the payload is returned for display.
///
/// # Errors
/// When persisting fails, or `send` fails (after the claim is rolled back).
pub fn run(
    ledger: &mut Ledger,
    now: DateTime<Utc>,
    policy: DigestPolicy,
    post: bool,
    persist: &mut dyn FnMut(&Ledger) -> Result<(), String>,
    send: &mut dyn FnMut(&str) -> Result<(), String>,
) -> Result<(DigestOutcome, Option<DigestPayload>), String> {
    if settle_stale_claim(ledger) {
        persist(ledger)?;
    }
    let Some(selection) = select(ledger, now, policy) else {
        return Ok((DigestOutcome::Skipped, None));
    };
    let body = payload(ledger, &selection, now, policy);
    if !post {
        return Ok((
            DigestOutcome::WouldSend {
                lines: body.lines(),
            },
            Some(body),
        ));
    }
    let text = serde_json::to_string_pretty(&body).map_err(|error| error.to_string())?;
    let ids = selection.ids();
    let shared: Vec<String> = selection.shared.keys().cloned().collect();
    ledger.digest_claim = Some(DigestClaim {
        claimed_at: now,
        ids: ids.clone(),
        shared: shared.clone(),
    });
    persist(ledger)?;
    match send(&text) {
        Ok(()) => {
            ledger.digest_claim = None;
            mark_sent(ledger, &ids, &shared, now);
            persist(ledger)?;
            Ok((
                DigestOutcome::Sent {
                    lines: body.lines(),
                },
                Some(body),
            ))
        }
        Err(error) => {
            ledger.digest_claim = None;
            persist(ledger)?;
            Err(format!("digest command failed; claim rolled back: {error}"))
        }
    }
}
