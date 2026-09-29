//! The hourly digest: at most one per interval, carrying only flags that have
//! held on the same head for the minimum age and have not been sent before.
//!
//! Delivery is claim-then-send. The claim is persisted before the configured
//! command runs; a failed command rolls the claim back so the next pass
//! retries; a claim found at start (the process died after claiming) is
//! treated as delivered, so a lost state write never double-posts.
//!
//! The payload is the `shipyard.pr-watch.digest/v1` JSON contract, written to
//! the command's stdin. An empty digest is never sent.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::flags::FlagKind;
use super::ledger::{DigestClaim, Ledger};

/// Digest schema identifier.
pub const DIGEST_SCHEMA: &str = "shipyard.pr-watch.digest/v1";

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DigestPayload {
    /// [`DIGEST_SCHEMA`].
    pub schema: String,
    /// `OWNER/REPO`.
    pub repo: String,
    /// When it was built.
    pub generated_at: String,
    /// Selection window.
    pub window: DigestWindow,
    /// The flags.
    pub flags: Vec<DigestFlag>,
}

/// `window` object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DigestWindow {
    /// Minimum flag age, minutes.
    pub min_age_minutes: i64,
}

/// One digest flag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DigestFlag {
    /// Pull request.
    pub pr: u64,
    /// Title.
    pub title: String,
    /// URL.
    pub url: String,
    /// Contract kind name.
    pub kind: String,
    /// Verdict.
    pub verdict: String,
    /// Evidence line.
    pub evidence: String,
    /// Episode start.
    pub first_seen_at: String,
    /// Age, minutes.
    pub age_minutes: i64,
    /// Head the flag holds on.
    pub head_sha: String,
}

fn stamp(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Entry ids a digest at `now` would carry. `None` when the interval has not
/// elapsed or nothing qualifies.
#[must_use]
pub fn select(ledger: &Ledger, now: DateTime<Utc>, policy: DigestPolicy) -> Option<Vec<String>> {
    if ledger
        .last_digest_at
        .is_some_and(|last| now - last < policy.interval)
    {
        return None;
    }
    let eligible = |kind_ok: &dyn Fn(FlagKind) -> bool| {
        ledger
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.addressed_at.is_none()
                    && entry.digested_at.is_none()
                    && now - entry.first_seen_at >= policy.min_age
                    && kind_ok(entry.kind)
            })
            .map(|(id, entry)| (id.clone(), entry.pr))
            .collect::<Vec<_>>()
    };
    let primary = eligible(&|kind| kind != FlagKind::SplitCandidate);
    if primary.is_empty() {
        return None;
    }
    let prs: BTreeSet<u64> = primary.iter().map(|(_, pr)| *pr).collect();
    let mut ids: Vec<String> = primary.into_iter().map(|(id, _)| id).collect();
    // The split advisory never travels alone: only beside another flag on the
    // same pull request in this digest.
    ids.extend(
        eligible(&|kind| kind == FlagKind::SplitCandidate)
            .into_iter()
            .filter(|(_, pr)| prs.contains(pr))
            .map(|(id, _)| id),
    );
    ids.sort();
    Some(ids)
}

/// Build the payload for `ids`.
#[must_use]
pub fn payload(
    ledger: &Ledger,
    ids: &[String],
    now: DateTime<Utc>,
    policy: DigestPolicy,
) -> DigestPayload {
    let mut flags: Vec<DigestFlag> = ids
        .iter()
        .filter_map(|id| ledger.entries.get(id))
        .map(|entry| DigestFlag {
            pr: entry.pr,
            title: entry.title.clone(),
            url: entry.url.clone(),
            kind: entry.kind.as_str().to_owned(),
            verdict: entry.verdict.clone(),
            evidence: entry.evidence.clone(),
            first_seen_at: stamp(entry.first_seen_at),
            age_minutes: (now - entry.first_seen_at).num_minutes(),
            head_sha: entry.head_sha.clone(),
        })
        .collect();
    flags.sort_by(|a, b| a.pr.cmp(&b.pr).then_with(|| a.kind.cmp(&b.kind)));
    DigestPayload {
        schema: DIGEST_SCHEMA.to_owned(),
        repo: ledger.repo.clone(),
        generated_at: stamp(now),
        window: DigestWindow {
            min_age_minutes: policy.min_age.num_minutes(),
        },
        flags,
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
        /// Flags it would carry.
        flags: usize,
    },
    /// Delivered.
    Sent {
        /// Flags it carried.
        flags: usize,
    },
}

/// Settle a claim left by an interrupted attempt: treat it as delivered.
pub fn settle_stale_claim(ledger: &mut Ledger) -> bool {
    let Some(claim) = ledger.digest_claim.take() else {
        return false;
    };
    for id in &claim.ids {
        if let Some(entry) = ledger.entries.get_mut(id) {
            entry.digested_at = Some(claim.claimed_at);
        }
    }
    ledger.last_digest_at = Some(claim.claimed_at);
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
    let Some(ids) = select(ledger, now, policy) else {
        return Ok((DigestOutcome::Skipped, None));
    };
    let body = payload(ledger, &ids, now, policy);
    if !post {
        return Ok((
            DigestOutcome::WouldSend {
                flags: body.flags.len(),
            },
            Some(body),
        ));
    }
    let text = serde_json::to_string_pretty(&body).map_err(|error| error.to_string())?;
    ledger.digest_claim = Some(DigestClaim {
        claimed_at: now,
        ids: ids.clone(),
    });
    persist(ledger)?;
    match send(&text) {
        Ok(()) => {
            ledger.digest_claim = None;
            for id in &ids {
                if let Some(entry) = ledger.entries.get_mut(id) {
                    entry.digested_at = Some(now);
                }
            }
            ledger.last_digest_at = Some(now);
            persist(ledger)?;
            Ok((
                DigestOutcome::Sent {
                    flags: body.flags.len(),
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
