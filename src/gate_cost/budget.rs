//! A bound on the GitHub reads one `metrics gate-cost` run may send.
//!
//! A 7-day window over a busy repository is over a thousand reads on the
//! shared App budget. [`ReadBudget`] wraps the reader: after `max` reads it
//! refuses more, with an error every caller recognises ([`is_stopped`]) and
//! degrades to a partial result instead of failing; `--cached-only` is a
//! budget of zero. [`estimate`] prices a window before it starts.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Default for `--max-reads`.
pub const DEFAULT_MAX_READS: u64 = 300;

/// Prefix of the error a refused read returns.
pub const STOPPED_PREFIX: &str = "reads stopped:";

/// Whether `error` is a read the budget refused (as opposed to a GitHub or
/// parse failure).
#[must_use]
pub fn is_stopped(error: &str) -> bool {
    error.contains(STOPPED_PREFIX)
}

/// Counts reads sent to GitHub and refuses them past the cap.
#[derive(Debug)]
pub struct ReadBudget {
    max: u64,
    cached_only: bool,
    sent: AtomicU64,
    stopped: AtomicBool,
}

impl ReadBudget {
    /// At most `max` reads; none at all when `cached_only`.
    #[must_use]
    pub const fn new(max: u64, cached_only: bool) -> Self {
        Self {
            max,
            cached_only,
            sent: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        }
    }

    /// Admit one read, or say why not. Call before sending each request.
    ///
    /// # Errors
    /// The cap is reached or the run is cached-only; the message starts with
    /// [`STOPPED_PREFIX`].
    pub fn admit(&self) -> Result<(), String> {
        if self.cached_only {
            self.stopped.store(true, Ordering::Relaxed);
            return Err(format!(
                "{STOPPED_PREFIX} --cached-only sends no GitHub reads"
            ));
        }
        // Claim a slot atomically so parallel readers never exceed the cap.
        let claimed = self
            .sent
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |sent| {
                (sent < self.max).then_some(sent + 1)
            });
        if claimed.is_err() {
            self.stopped.store(true, Ordering::Relaxed);
            return Err(format!(
                "{STOPPED_PREFIX} the --max-reads cap of {} was reached",
                self.max
            ));
        }
        Ok(())
    }

    /// Reads admitted so far.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Whether any read was refused: the result is partial.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Why the result is partial, when it is.
    #[must_use]
    pub fn partial_reason(&self) -> Option<String> {
        self.stopped().then(|| {
            if self.cached_only {
                "cached-only: only answers already in the local cache were used; runs and \
                 signals that were not cached are missing"
                    .to_owned()
            } else {
                format!(
                    "stopped at the --max-reads cap of {} ({} reads sent); runs and signals not \
                     read are missing, so the figures understate the window",
                    self.max,
                    self.sent()
                )
            }
        })
    }
}

/// Reads a window costs at most (the cache is ignored): per event the run
/// listing pages, the cross-check walk (up to `WALK_MAX_PAGES` pages) and one
/// jobs read per run; two annotation reads per merge-group run; the batch
/// first-parent walk, up to `MAX_BATCH_WALK` commit reads for each merge-queue
/// push (bounded by the merge-group runs, one push per batch at most); and a
/// fixed handful for the ruleset, merged pull requests, activity feed, queue
/// depth and runner census.
#[must_use]
pub fn estimate(runs_per_event: &[(String, u64)]) -> u64 {
    const FIXED: u64 = 10;
    const LISTING_PAGE: u64 = 100;
    let walk_pages = u64::from(super::WALK_MAX_PAGES);
    let batch_walk = u64::from(super::MAX_BATCH_WALK);
    runs_per_event
        .iter()
        .map(|(event, runs)| {
            let merge_group = event == super::MERGE_GROUP_EVENT;
            let per_run = if merge_group { 3 } else { 1 };
            let batches = if merge_group { runs * batch_walk } else { 0 };
            runs.div_ceil(LISTING_PAGE) + walk_pages + runs * per_run + batches
        })
        .sum::<u64>()
        + FIXED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_admits_exactly_n_reads_then_refuses_and_marks_partial() {
        let budget = ReadBudget::new(3, false);
        for _ in 0..3 {
            assert_eq!(budget.admit(), Ok(()));
        }
        assert!(!budget.stopped());
        let refused = budget.admit().expect_err("fourth read");
        assert!(is_stopped(&refused), "{refused}");
        assert_eq!(budget.sent(), 3);
        assert!(budget.stopped());
        assert!(
            budget
                .partial_reason()
                .is_some_and(|why| why.contains("cap of 3"))
        );
    }

    #[test]
    fn cached_only_admits_nothing() {
        let budget = ReadBudget::new(300, true);
        assert!(budget.admit().is_err());
        assert_eq!(budget.sent(), 0);
        assert!(
            budget
                .partial_reason()
                .is_some_and(|why| why.contains("cached-only"))
        );
    }

    #[test]
    fn the_estimate_counts_listing_pages_jobs_and_annotations() {
        let runs = [
            ("pull_request".to_owned(), 250),
            (super::super::MERGE_GROUP_EVENT.to_owned(), 100),
        ];
        // pull_request: 3 pages + 10 walk + 250 jobs.
        // merge_group: 1 page + 10 walk + 300 jobs/annotations + 100 * 50 walk.
        // 10 fixed.
        assert_eq!(estimate(&runs), 263 + 5_311 + 10);
    }

    #[test]
    fn a_window_of_no_runs_still_prices_the_cross_check_walk() {
        let runs = [("pull_request".to_owned(), 0)];
        assert_eq!(estimate(&runs), 10 + 10);
    }
}
