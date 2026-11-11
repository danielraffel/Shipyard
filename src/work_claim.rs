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
        let current = self.leases.get_mut(&key).ok_or(ClaimError::Missing)?;
        current.heartbeat_at = now;
        current.expires_at = now + self.ttl;
        current.token = token;
        Ok(current.clone())
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 11, 12, minutes as u32, 0)
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
}
