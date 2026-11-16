//! Lease decisions for fleet work claims.
//!
//! The registry backend is deliberately separate from the decision core. The
//! fleet will persist these records through the existing PR owner registry
//! (the GitHub-visible marker plus its steward/relay projection), rather than
//! creating another host-local claim directory. Keeping the transition rules
//! pure makes expiry, duplicate ownership, and push preconditions testable
//! without spending GitHub API budget.

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};

use chrono::{DateTime, Duration, Utc};

/// A claim is unique for one repository, PR head, and work kind.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ClaimKey {
    pub(crate) repo: String,
    pub(crate) pr: u64,
    pub(crate) head_sha: String,
    pub(crate) kind: String,
}

/// The current holder and expiry of a claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkLease {
    pub(crate) key: ClaimKey,
    pub(crate) holder: String,
    pub(crate) token: String,
    pub(crate) heartbeat_at: DateTime<Utc>,
    pub(crate) expires_at: DateTime<Utc>,
}

/// Why a claim or push precondition was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ClaimError {
    AlreadyClaimed {
        holder: String,
        expires_at: DateTime<Utc>,
    },
    Missing,
    Expired,
    TokenMismatch,
    HeadMismatch,
    GatesMissing,
}

impl Display for ClaimError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyClaimed { holder, expires_at } => write!(
                formatter,
                "claim is held by {holder} until {}",
                expires_at.to_rfc3339()
            ),
            Self::Missing => formatter.write_str("claim is missing"),
            Self::Expired => formatter.write_str("claim lease expired"),
            Self::TokenMismatch => formatter.write_str("claim token does not match holder"),
            Self::HeadMismatch => formatter.write_str("claim head does not match push head"),
            Self::GatesMissing => formatter.write_str("push gates did not pass"),
        }
    }
}

/// In-memory claim decision core. A durable adapter must serialize the same
/// key and perform an atomic compare-and-swap before publishing a lease.
#[derive(Debug)]
pub(crate) struct ClaimRegistry {
    ttl: Duration,
    leases: BTreeMap<ClaimKey, WorkLease>,
}

#[allow(dead_code)]
impl ClaimRegistry {
    pub(crate) fn new(ttl: Duration) -> Self {
        assert!(ttl > Duration::zero(), "claim TTL must be positive");
        Self {
            ttl,
            leases: BTreeMap::new(),
        }
    }

    /// Claim a key, or renew an existing claim held by this holder.
    pub(crate) fn acquire(
        &mut self,
        key: ClaimKey,
        holder: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<WorkLease, ClaimError> {
        let holder = holder.into();
        if let Some(current) = self.leases.get(&key) {
            if current.expires_at > now && current.holder != holder {
                return Err(ClaimError::AlreadyClaimed {
                    holder: current.holder.clone(),
                    expires_at: current.expires_at,
                });
            }
            if current.expires_at > now && current.holder == holder {
                return self.renew(&key, current.token.clone(), now);
            }
        }
        let lease = WorkLease {
            key: key.clone(),
            token: format!("{holder}:{}", now.timestamp_nanos_opt().unwrap_or_default()),
            holder,
            heartbeat_at: now,
            expires_at: now + self.ttl,
        };
        self.leases.insert(key, lease.clone());
        Ok(lease)
    }

    /// Renew a live lease. A silent holder cannot revive an expired claim.
    pub(crate) fn heartbeat(
        &mut self,
        key: &ClaimKey,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkLease, ClaimError> {
        let current = self.leases.get(key).ok_or(ClaimError::Missing)?;
        if current.expires_at <= now {
            return Err(ClaimError::Expired);
        }
        if current.token != token {
            return Err(ClaimError::TokenMismatch);
        }
        self.renew(key, token.to_owned(), now)
    }

    /// Release a live lease held by `token`.
    pub(crate) fn release(
        &mut self,
        key: &ClaimKey,
        token: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimError> {
        let current = self.leases.get(key).ok_or(ClaimError::Missing)?;
        if current.expires_at <= now {
            return Err(ClaimError::Expired);
        }
        if current.token != token {
            return Err(ClaimError::TokenMismatch);
        }
        self.leases.remove(key);
        Ok(())
    }

    /// Validate the final pre-push facts against the exact lease.
    pub(crate) fn verify_push(
        &self,
        key: &ClaimKey,
        token: &str,
        actual_head_sha: &str,
        gates_passed: bool,
        now: DateTime<Utc>,
    ) -> Result<(), ClaimError> {
        let current = self.leases.get(key).ok_or(ClaimError::Missing)?;
        if current.expires_at <= now {
            return Err(ClaimError::Expired);
        }
        if current.token != token {
            return Err(ClaimError::TokenMismatch);
        }
        if key.head_sha != actual_head_sha {
            return Err(ClaimError::HeadMismatch);
        }
        if !gates_passed {
            return Err(ClaimError::GatesMissing);
        }
        Ok(())
    }

    fn renew(
        &mut self,
        key: &ClaimKey,
        token: String,
        now: DateTime<Utc>,
    ) -> Result<WorkLease, ClaimError> {
        let current = self.leases.get_mut(key).ok_or(ClaimError::Missing)?;
        current.heartbeat_at = now;
        current.expires_at = now + self.ttl;
        current.token = token;
        Ok(current.clone())
    }
}

/// One `repair-claim: <owner> [(<note>)] until <time>` line as written.
///
/// This is the only parser of the claim grammar: the relay, `shipyard
/// advance` and the push fence all read claims through it. The optional
/// parenthetical note is what people actually write
/// (`repair-claim: team-lead (repair-462-allowlist) until 2026-11-16T22:00Z`).
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClaimLine {
    pub(crate) owner: String,
    pub(crate) until: String,
}

/// Every claim line in `body`, in order.
#[cfg(test)]
pub(crate) fn claim_lines(body: &str) -> Vec<ClaimLine> {
    claim_acts(body)
        .into_iter()
        .filter_map(|act| match act {
            ClaimAct::Claim { owner, until } => Some(ClaimLine { owner, until }),
            ClaimAct::Release { .. } | ClaimAct::Take { .. } => None,
        })
        .collect()
}

/// A claim's expiry, in every spelling fleet-handbook's `repair_preclaim.py`
/// reads plus a few it does not: `2026-11-16T22:00Z`, a space for the `T`,
/// optional seconds (and fraction), a `Z`, ` UTC` or `+HH:MM` suffix or none
/// (read as UTC), and a trailing period. `None` when it is none of these.
pub(crate) fn claim_expiry(until: &str) -> Option<DateTime<Utc>> {
    let text = until.trim().trim_end_matches('.').trim_end();
    let (text, zoned) = if let Some(rest) = text.strip_suffix("UTC") {
        (rest.trim_end(), false)
    } else if let Some(rest) = text.strip_suffix('Z') {
        (rest, false)
    } else {
        (text, true)
    };
    let text = text.replacen(' ', "T", 1);
    if zoned {
        for format in ["%Y-%m-%dT%H:%M%:z", "%Y-%m-%dT%H:%M:%S%.f%:z"] {
            if let Ok(time) = DateTime::parse_from_str(&text, format) {
                return Some(time.with_timezone(&Utc));
            }
        }
    }
    ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S%.f"]
        .iter()
        .find_map(|format| chrono::NaiveDateTime::parse_from_str(&text, format).ok())
        .map(|time| time.and_utc())
}

/// What one comment line does to the claims on a pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ClaimAct {
    Claim {
        owner: String,
        until: String,
    },
    /// `None`: a bare `repair-claim-released:`, which ends every earlier claim
    /// (as `repair_preclaim.py` reads it).
    Release {
        owner: Option<String>,
    },
    Take {
        owner: String,
        from: String,
    },
}

/// The claim acts in `body`, one per line at most, in line order.
fn claim_acts(body: &str) -> Vec<ClaimAct> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(concat!(
            r"^[ \t]*(?:",
            r"repair-claim:[ \t]*(\S+)(?:[ \t]+\([^)]*\))?[ \t]+until[ \t]+",
            r"(\d{4}-\d\d-\d\d[T ]\d\d:\d\d(?::\d\d(?:\.\d+)?)?(?:[ \t]*(?:Z|UTC|[+-]\d\d:\d\d))?\.?|\S+)",
            r"|repair-claim-released:[ \t]*(\S+)?",
            r"|repair-claim-taken:[ \t]*(\S+)[ \t]+from[ \t]+(\S+)",
            r")",
        ))
        .expect("regex")
    });
    body.lines()
        .filter_map(|line| {
            let found = re.captures(line)?;
            let group = |index: usize| found.get(index).map(|m| m.as_str().to_owned());
            if let (Some(owner), Some(until)) = (group(1), group(2)) {
                Some(ClaimAct::Claim { owner, until })
            } else if let Some(owner) = group(4) {
                Some(ClaimAct::Take {
                    owner,
                    from: group(5)?,
                })
            } else {
                Some(ClaimAct::Release { owner: group(3) })
            }
        })
        .collect()
}

/// A claim someone holds: posted, and neither released nor taken since.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HeldClaim {
    pub(crate) owner: String,
    pub(crate) until: String,
    /// The comment that put the claim in this owner's hands.
    pub(crate) comment_id: u64,
    /// When the claim itself was posted (a take keeps the original time).
    pub(crate) claimed_at: String,
    /// Order of the act that put it in these hands, oldest first.
    pub(crate) seq: usize,
}

/// A held claim that is live at the time asked about.
pub(crate) type LiveClaim = HeldClaim;

impl HeldClaim {
    /// When the claim ends. An unreadable time ends `unreadable` after the
    /// claim was posted, or never (`None`).
    pub(crate) fn expires(&self, unreadable: Option<Duration>) -> Option<DateTime<Utc>> {
        claim_expiry(&self.until).or_else(|| {
            let posted = DateTime::parse_from_rfc3339(&self.claimed_at).ok()?;
            Some(posted.with_timezone(&Utc) + unreadable?)
        })
    }

    pub(crate) fn live_at(&self, now: DateTime<Utc>, unreadable: Option<Duration>) -> bool {
        self.expires(unreadable).is_none_or(|until| until > now)
    }
}

/// One trusted comment, as the claim fold reads it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClaimComment<'a> {
    pub(crate) id: u64,
    /// GitHub's `created_at` (RFC 3339, UTC), which orders the comments.
    pub(crate) at: &'a str,
    pub(crate) body: &'a str,
}

/// The claims held after trusted comments in any order, expired or not.
///
/// Comments are read oldest first (stable for equal times). A
/// `repair-claim-released: <owner>` ends that owner's earlier claim, and a
/// bare `repair-claim-released:` every earlier claim; a
/// `repair-claim-taken: <new> from <old>` moves the old owner's earlier claim,
/// with its expiry, to the new owner. A claim posted after either is held
/// again.
pub(crate) fn held_claims(comments: &[ClaimComment<'_>]) -> Vec<HeldClaim> {
    let mut ordered: Vec<&ClaimComment<'_>> = comments.iter().collect();
    ordered.sort_by(|left, right| left.at.cmp(right.at));
    let mut held: BTreeMap<String, HeldClaim> = BTreeMap::new();
    let mut seq = 0;
    for comment in ordered {
        for act in claim_acts(comment.body) {
            seq += 1;
            match act {
                ClaimAct::Claim { owner, until } => {
                    held.insert(
                        owner.clone(),
                        HeldClaim {
                            owner,
                            until,
                            comment_id: comment.id,
                            claimed_at: comment.at.to_owned(),
                            seq,
                        },
                    );
                }
                ClaimAct::Release { owner: Some(owner) } => {
                    held.remove(&owner);
                }
                ClaimAct::Release { owner: None } => held.clear(),
                ClaimAct::Take { owner, from } => {
                    if let Some(taken) = held.remove(&from) {
                        held.insert(
                            owner.clone(),
                            HeldClaim {
                                owner,
                                comment_id: comment.id,
                                seq,
                                ..taken
                            },
                        );
                    }
                }
            }
        }
    }
    let mut held: Vec<HeldClaim> = held.into_values().collect();
    held.sort_by_key(|claim| claim.seq);
    held
}

/// The claims held in comment records shaped as `shipyard advance` reads
/// them (`id`, `type`, `association`, `at`, `body`). Only a trusted author's
/// comment counts: a repository owner, member or collaborator, or a bot (the
/// fleet's lanes post through its App).
pub(crate) fn held_claims_in_records(comments: &[serde_json::Value]) -> Vec<HeldClaim> {
    let trusted: Vec<ClaimComment<'_>> = comments
        .iter()
        .filter(|comment| trusted_record(comment))
        .map(|comment| ClaimComment {
            id: comment
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
            at: comment
                .get("at")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
            body: comment
                .get("body")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
        })
        .collect();
    held_claims(&trusted)
}

/// [`held_claims_in_records`] live at `now`, an unreadable time live.
pub(crate) fn live_claims_in_records(
    comments: &[serde_json::Value],
    now: DateTime<Utc>,
) -> Vec<LiveClaim> {
    held_claims_in_records(comments)
        .into_iter()
        .filter(|claim| claim.live_at(now, None))
        .collect()
}

fn trusted_record(comment: &serde_json::Value) -> bool {
    comment
        .get("association")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|association| {
            crate::head_approval::TRUSTED_ASSOCIATIONS.contains(&association)
        })
        || comment.get("type").and_then(serde_json::Value::as_str) == Some("Bot")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(minutes: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, minutes, 0)
            .single()
            .expect("timestamp")
    }

    fn key() -> ClaimKey {
        ClaimKey {
            repo: "danielraffel/Shipyard".to_owned(),
            pr: 1234,
            head_sha: "a".repeat(40),
            kind: "repair".to_owned(),
        }
    }

    #[test]
    fn a_duplicate_live_claim_is_refused() {
        let mut registry = ClaimRegistry::new(Duration::minutes(10));
        let first = registry
            .acquire(key(), "m5s-lead", at(0))
            .expect("first claim");
        let duplicate = registry.acquire(key(), "orchestrator-job", at(1));
        assert_eq!(
            duplicate,
            Err(ClaimError::AlreadyClaimed {
                holder: first.holder,
                expires_at: at(10),
            })
        );
    }

    #[test]
    fn an_expired_claim_can_be_reclaimed() {
        let mut registry = ClaimRegistry::new(Duration::minutes(10));
        let first = registry
            .acquire(key(), "m5s-lead", at(0))
            .expect("first claim");
        let reclaimed = registry
            .acquire(key(), "orchestrator-job", at(10))
            .expect("expired claim is reclaimable");
        assert_ne!(reclaimed.token, first.token);
        assert_eq!(reclaimed.holder, "orchestrator-job");
    }

    #[test]
    fn final_push_requires_live_token_exact_head_and_gates() {
        let mut registry = ClaimRegistry::new(Duration::minutes(10));
        let key = key();
        let lease = registry.acquire(key.clone(), "lead", at(0)).expect("claim");
        assert_eq!(
            registry.verify_push(&key, "other-token", &key.head_sha, true, at(1)),
            Err(ClaimError::TokenMismatch)
        );
        assert_eq!(
            registry.verify_push(&key, &lease.token, &"b".repeat(40), true, at(1)),
            Err(ClaimError::HeadMismatch)
        );
        assert_eq!(
            registry.verify_push(&key, &lease.token, &key.head_sha, false, at(1)),
            Err(ClaimError::GatesMissing)
        );
        registry
            .verify_push(&key, &lease.token, &key.head_sha, true, at(1))
            .expect("all push preconditions hold");
    }

    #[test]
    fn the_claim_grammar_reads_a_note_and_a_minute_time() {
        let body =
            "context\nrepair-claim: team-lead (repair-462-allowlist) until 2026-11-16T22:00Z\n";
        assert_eq!(
            claim_lines(body),
            vec![ClaimLine {
                owner: "team-lead".to_owned(),
                until: "2026-11-16T22:00Z".to_owned(),
            }]
        );
        assert_eq!(
            claim_expiry("2026-11-16T22:00Z"),
            Utc.with_ymd_and_hms(2026, 11, 16, 22, 0, 0).single()
        );
        assert_eq!(claim_expiry("tonight"), None);
        assert!(claim_lines("repair-claim-released: team-lead").is_empty());
        assert!(
            claim_lines("see repair-claim: x until y").is_empty(),
            "line start only"
        );
    }

    /// Each real spelling, with the owner and expiry fleet-handbook's
    /// `repair_preclaim.py` (origin/main) reads from it, computed by running
    /// its `CLAIM` and `epoch`. `None` where preclaim does not match.
    #[test]
    fn every_spelling_reads_as_repair_preclaim_reads_it() {
        let cases: [(&str, Option<&str>, &str, Option<&str>); 8] = [
            // Generous-Corp/vellum#462 verbatim; preclaim misses the note.
            (
                "repair-claim: team-lead (repair-462-allowlist) until 2026-11-16T22:00Z",
                None,
                "team-lead",
                Some("2026-11-16T22:00:00+00:00"),
            ),
            // fleet-pass.sh's own form, and preclaim's test FAR.
            (
                "repair-claim: repair-lane/repair-2 until 2099-01-01T00:00Z",
                Some("2099-01-01T00:00:00+00:00"),
                "repair-lane/repair-2",
                Some("2099-01-01T00:00:00+00:00"),
            ),
            (
                "repair-claim: repair-lane/repair-2 until 2026-11-16 22:00Z",
                Some("2026-11-16T22:00:00+00:00"),
                "repair-lane/repair-2",
                Some("2026-11-16T22:00:00+00:00"),
            ),
            (
                "repair-claim: repair-lane/repair-2 until 2026-11-16T22:00 UTC",
                Some("2026-11-16T22:00:00+00:00"),
                "repair-lane/repair-2",
                Some("2026-11-16T22:00:00+00:00"),
            ),
            (
                "repair-claim: repair-lane/repair-2 until 2026-11-16T22:00UTC",
                Some("2026-11-16T22:00:00+00:00"),
                "repair-lane/repair-2",
                Some("2026-11-16T22:00:00+00:00"),
            ),
            (
                "repair-claim: repair-lane/repair-2 until 2026-11-16T22:00:30Z",
                Some("2026-11-16T22:00:30+00:00"),
                "repair-lane/repair-2",
                Some("2026-11-16T22:00:30+00:00"),
            ),
            (
                "repair-claim: repair-lane/repair-2 until 2026-11-16T22:00Z.",
                Some("2026-11-16T22:00:00+00:00"),
                "repair-lane/repair-2",
                Some("2026-11-16T22:00:00+00:00"),
            ),
            // No zone: read as UTC here; preclaim does not match it.
            (
                "repair-claim: repair-lane/repair-2 until 2026-11-16T22:00",
                None,
                "repair-lane/repair-2",
                Some("2026-11-16T22:00:00+00:00"),
            ),
        ];
        for (line, preclaim, owner, ours) in cases {
            let found = claim_lines(line);
            assert_eq!(found.len(), 1, "{line}");
            assert_eq!(found[0].owner, owner, "{line}");
            let expiry = claim_expiry(&found[0].until).map(|time| time.to_rfc3339());
            assert_eq!(expiry.as_deref(), ours, "{line}");
            if let Some(preclaim) = preclaim {
                assert_eq!(
                    expiry.as_deref(),
                    Some(preclaim),
                    "differs from preclaim: {line}"
                );
            }
        }
    }
}
