//! Proof that something consumes the store: the worker's last-run marker.
//!
//! The steward accepts recovery requests only while a worker has run recently,
//! and an invariant fails when old unattempted requests sit with no run. A
//! queue nothing drains would otherwise fill silently.

use super::archive::persist_json;
use super::{DateTime, RecoveryResult, RecoveryStore, Utc, fs};
use serde::{Deserialize, Serialize};

/// A worker run older than this (or none) means no consumer.
pub const WORKER_STALE_AFTER_MINUTES: i64 = 180;
const LAST_RUN_FILE: &str = "worker-last-run";
const LAST_RUN_SCHEMA: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkerRun {
    schema_version: u32,
    at: DateTime<Utc>,
}

/// Whether the last worker run is missing or too old for the store to be
/// considered consumed.
#[must_use]
pub fn worker_is_stale(last_run: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last_run
        .is_none_or(|at| now.signed_duration_since(at).num_minutes() >= WORKER_STALE_AFTER_MINUTES)
}

impl RecoveryStore {
    /// Record that a worker ran `--apply` at `now`, whether or not it found
    /// work. Written atomically; no store lock is needed.
    pub fn record_worker_run(&self, now: DateTime<Utc>) -> RecoveryResult<()> {
        persist_json(
            &self.root.join(LAST_RUN_FILE),
            &WorkerRun {
                schema_version: LAST_RUN_SCHEMA,
                at: now,
            },
        )
    }

    /// When a worker last ran, or `None` when none has (or the marker is
    /// unreadable, which counts as no consumer).
    #[must_use]
    pub fn last_worker_run(&self) -> Option<DateTime<Utc>> {
        let payload = fs::read(self.root.join(LAST_RUN_FILE)).ok()?;
        let run = serde_json::from_slice::<WorkerRun>(&payload).ok()?;
        (run.schema_version == LAST_RUN_SCHEMA).then_some(run.at)
    }

    /// The creation time of the oldest unattempted pending request, read
    /// without writing.
    pub fn oldest_unattempted_pending(&self) -> RecoveryResult<Option<DateTime<Utc>>> {
        Ok(self
            .pending_read_only(super::MAX_PENDING_LIMIT)?
            .iter()
            .filter(|record| record.receipt.attempt == 0)
            .map(|record| record.request.created_at)
            .min())
    }
}
