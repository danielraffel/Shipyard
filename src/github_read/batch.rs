//! A GraphQL batch size that follows measured latency.
//!
//! The steward reads current-head check rollups several heads per query. A
//! fixed batch is either too small on a quiet host (more round trips than
//! needed) or too large on a loaded one (a slow query times out and every head
//! in it falls back to REST). The sizer grows the batch while queries answer
//! quickly and halves it the moment one is slow or fails. Each steward pass is
//! its own process, so the size is kept in a small state file between passes.
//!
//! Bounds: the size never leaves `[MIN_BATCH, MAX_BATCH]`, and it starts at
//! [`DEFAULT_BATCH`], the fixed size used before.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Starting size: the fixed rollup batch the steward used before.
pub(crate) const DEFAULT_BATCH: usize = 6;
pub(crate) const MIN_BATCH: usize = 2;
pub(crate) const MAX_BATCH: usize = 12;
/// A pass whose slowest-but-one batch answered under this grows the size.
const GROW_BELOW: Duration = Duration::from_millis(2_500);
/// Any batch slower than this, or failing, halves the size.
const SHRINK_ABOVE: Duration = Duration::from_secs(5);
const STATE_FILE: &str = "github-read-batch.json";

#[derive(Debug, Default, Deserialize, Serialize)]
struct Persisted {
    heads_per_query: usize,
}

#[derive(Debug, Default)]
struct Observations {
    latencies: Vec<Duration>,
    failed: bool,
}

/// The current batch size and this pass's observations.
#[derive(Debug)]
pub(crate) struct BatchSizer {
    path: Option<PathBuf>,
    size: usize,
    observed: Mutex<Observations>,
}

impl BatchSizer {
    /// A sizer that starts from, and saves to, `state_dir`.
    pub(crate) fn persistent(state_dir: &Path) -> Self {
        let path = state_dir.join(STATE_FILE);
        let size = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Persisted>(&bytes).ok())
            .map_or(DEFAULT_BATCH, |persisted| persisted.heads_per_query)
            .clamp(MIN_BATCH, MAX_BATCH);
        Self {
            path: Some(path),
            size,
            observed: Mutex::default(),
        }
    }

    /// A sizer that keeps nothing between processes.
    #[cfg(test)]
    pub(crate) fn in_memory(size: usize) -> Self {
        Self {
            path: None,
            size: size.clamp(MIN_BATCH, MAX_BATCH),
            observed: Mutex::default(),
        }
    }

    /// Heads per query for this pass.
    pub(crate) fn size(&self) -> usize {
        self.size
    }

    /// Record one batch's latency and outcome.
    pub(crate) fn record(&self, elapsed: Duration, ok: bool) {
        if let Ok(mut observed) = self.observed.lock() {
            observed.latencies.push(elapsed);
            observed.failed |= !ok;
        }
    }

    /// The size the next pass should use, from this pass's observations.
    pub(crate) fn next_size(&self) -> usize {
        let Ok(observed) = self.observed.lock() else {
            return self.size;
        };
        next_size(self.size, &observed.latencies, observed.failed)
    }

    /// Save [`Self::next_size`] for the next pass. Nothing observed, nothing
    /// saved.
    pub(crate) fn save(&self) -> Result<usize, String> {
        let next = self.next_size();
        let observed_any = self
            .observed
            .lock()
            .is_ok_and(|observed| !observed.latencies.is_empty());
        let Some(path) = &self.path else {
            return Ok(next);
        };
        if !observed_any {
            return Ok(next);
        }
        let bytes = serde_json::to_vec(&Persisted {
            heads_per_query: next,
        })
        .map_err(|error| error.to_string())?;
        crate::durable_file::replace(path, &bytes)
            .map_err(|error| format!("could not save {}: {error}", path.display()))?;
        Ok(next)
    }
}

/// Grow by two while the 90th-percentile batch answers under
/// [`GROW_BELOW`]; halve on any failure or any batch over [`SHRINK_ABOVE`].
pub(crate) fn next_size(current: usize, latencies: &[Duration], failed: bool) -> usize {
    if latencies.is_empty() {
        return current.clamp(MIN_BATCH, MAX_BATCH);
    }
    let slowest = latencies.iter().max().copied().unwrap_or_default();
    let next = if failed || slowest > SHRINK_ABOVE {
        current / 2
    } else if percentile(latencies, 90) < GROW_BELOW {
        current + 2
    } else {
        current
    };
    next.clamp(MIN_BATCH, MAX_BATCH)
}

/// Nearest-rank percentile.
pub(crate) fn percentile(values: &[Duration], pct: usize) -> Duration {
    if values.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = values.to_vec();
    sorted.sort();
    let rank = (pct * sorted.len()).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}
