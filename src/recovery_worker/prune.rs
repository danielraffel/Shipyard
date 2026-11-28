//! Retire pending recovery requests that can no longer be acted on.
//!
//! The hot set is capped at [`MAX_ACTIVE_RECORDS`]. A pending request whose
//! pull request merged, closed or moved to a new head is never claimed (the
//! worker would only supersede it), so without a sweep those records fill the
//! cap and every later request is refused.

use super::archive::MAX_ACTIVE_RECORDS;
use super::{
    DateTime, RecoveryRecord, RecoveryResult, RecoveryStatus, RecoveryStore, Utc, validate_record,
};
use std::collections::BTreeMap;

/// How long an unattempted request may stay pending before it is retired.
/// A live head that is still red is published again by the next steward pass.
pub const MAX_PENDING_AGE_DAYS: i64 = 7;

/// What one prune pass saw and did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct PruneOutcome {
    /// Active records left after the pass.
    pub active: usize,
    /// The hot-set capacity.
    pub limit: usize,
    /// Retired because the pull request is closed, merged or on another head.
    pub retired_dead: usize,
    /// Retired because they sat unattempted past the age bound.
    pub retired_aged: usize,
}

impl PruneOutcome {
    /// Whether no further request can be accepted.
    #[must_use]
    pub const fn full(&self) -> bool {
        self.active >= self.limit
    }
}

impl RecoveryStore {
    /// Supersede unattempted pending records for `repo` and `base` whose pull
    /// request is absent from `open_heads` or has moved to another head, and
    /// every unattempted pending record older than `max_age`.
    ///
    /// `open_heads` must be the complete open-pull-request list for that base.
    /// A record retired by mistake is reactivated by the next enqueue of the
    /// same exact request, so a partial list costs one pass, not a failure.
    /// Claimed (`Running`) and attempted records are never touched.
    pub fn prune_unattempted(
        &self,
        repo: &str,
        base: &str,
        open_heads: &BTreeMap<u64, String>,
        now: DateTime<Utc>,
        max_age: chrono::Duration,
    ) -> RecoveryResult<PruneOutcome> {
        let _lock = self.lock()?;
        let mut outcome = PruneOutcome {
            limit: MAX_ACTIVE_RECORDS,
            ..PruneOutcome::default()
        };
        let mut retired = 0;
        let mut active = 0;
        for id in self.record_ids_unlocked()? {
            active += 1;
            let Some(record) = self.load_unlocked(&id)? else {
                continue;
            };
            if record.receipt.status != RecoveryStatus::Pending || record.receipt.attempt != 0 {
                continue;
            }
            let request = &record.request;
            let dead = request.repo.eq_ignore_ascii_case(repo)
                && request.base_ref == base
                && open_heads.get(&request.pr) != Some(&request.head_sha);
            let aged = now.signed_duration_since(request.created_at) > max_age;
            let (reason, counter) = if dead {
                (
                    "pull request is closed, merged or on another head; retired unattempted",
                    &mut outcome.retired_dead,
                )
            } else if aged {
                (
                    "unattempted past the pending age bound; retired",
                    &mut outcome.retired_aged,
                )
            } else {
                continue;
            };
            self.retire_unattempted_unlocked(record, reason, now)?;
            *counter += 1;
            retired += 1;
        }
        outcome.active = active - retired;
        Ok(outcome)
    }

    fn retire_unattempted_unlocked(
        &self,
        mut record: RecoveryRecord,
        detail: &str,
        now: DateTime<Utc>,
    ) -> RecoveryResult<()> {
        record.receipt.status = RecoveryStatus::Superseded;
        record.receipt.superseded_by = None;
        record.receipt.detail = Some(detail.to_owned());
        record.receipt.completed_at = Some(now);
        record.receipt.deferred_at = None;
        record.receipt.updated_at = now;
        validate_record(&record)?;
        self.save_unlocked(&record)
    }
}
