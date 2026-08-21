//! Opt-in, default-off daemon sweep that abandons orphaned in-flight
//! ship-states.
//!
//! When a ship worker dies without writing a verdict, its ship-state stays
//! in-flight forever — [`ship_terminal_verdict`] never resolves, so the
//! wait/auto-merge path blocks on a PR nobody is driving. The read-only
//! classifier ([`crate::ship_liveness`]) detects and *reports* this; this module
//! optionally *acts* on it, behind `[ship_state] auto_resume` (default off).
//!
//! The action is deliberately minimal and safe: mark the state terminally
//! **abandoned** (so the wait path stops and the PR is never merged) and surface
//! it — a human re-ships. There is no automatic re-dispatch, so there is no
//! resume→die→resume loop, and abandonment is a terminal fixed point (an
//! abandoned state is terminal, so it is never reclassified or re-abandoned).
//!
//! Fail-CLOSED and conservatively quantified: only the strongest evidence,
//! [`OrphanEvidence::QueueStale`] — a durable owning job whose heartbeat is
//! provably dead past the reaper threshold — abandons. A live/pending worker,
//! any weaker (inferred-absence / time-only) signal, an unavailable queue, or a
//! verdict that lands before the per-PR lock is taken all leave the state
//! untouched. Marking a *live* ship failed is the one catastrophic error, so the
//! bar is a provable dead worker AND an under-lock in-flight re-check.

use std::path::Path;

use chrono::{DateTime, Duration, Utc};

use crate::config::LoadedConfig;
use crate::ship_liveness::{
    OrphanEvidence, auto_resume_enabled, collect_orphans, orphan_stale_after, with_liveness_context,
};
use crate::ship_state::{AbandonRecord, ShipStateStore};
use crate::watch::ship_terminal_verdict;

/// One ship-state abandoned by a sweep, for reporting / daemon IPC.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbandonedShipState {
    /// Pull request number.
    pub pr: u64,
    /// Repository slug.
    pub repo: String,
    /// Orphan-evidence label that justified abandonment.
    pub evidence: String,
    /// Minutes the state had been idle (`updated_at` age) when abandoned.
    pub stalled_minutes: i64,
}

/// Summary of one abandon sweep.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AbandonReport {
    /// States marked terminally abandoned this pass.
    pub abandoned: Vec<AbandonedShipState>,
    /// Strong (`QueueStale`) candidates skipped because a verdict landed or the
    /// state was no longer a strong orphan by the time its per-PR lock was held.
    pub raced: usize,
}

/// Run one opt-in orphan-abandon sweep. A no-op — returns an empty report,
/// mutates nothing, and opens no queue — unless `[ship_state] auto_resume` is
/// enabled. `now` is injected for deterministic testing.
#[must_use]
pub fn sweep_orphaned_ship_states(
    state_dir: &Path,
    config: &LoadedConfig,
    now: DateTime<Utc>,
) -> AbandonReport {
    if !auto_resume_enabled(config) {
        return AbandonReport::default();
    }
    let Ok(store) = ShipStateStore::new(state_dir.join("ship")) else {
        return AbandonReport::default();
    };
    let stale_after = orphan_stale_after(config);
    // Detection pass: a cheap snapshot classification only decides which PRs are
    // *candidates*. The authoritative destructive decision is re-made per PR,
    // under its lock, against a fresh queue read (see `abandon_one`) — so a
    // worker that started or resumed since this snapshot is never abandoned.
    let strong: Vec<(String, u64)> = with_liveness_context(state_dir, stale_after, |liveness| {
        collect_orphans(&store, liveness, now)
            .into_iter()
            .filter(|(_, _, orphan)| orphan.evidence == OrphanEvidence::QueueStale)
            .map(|(repo, pr, _)| (repo, pr))
            .collect()
    });
    let mut report = AbandonReport::default();
    for (repo, pr) in strong {
        match abandon_one(&store, state_dir, stale_after, &repo, pr, now) {
            AbandonOutcome::Abandoned(entry) => report.abandoned.push(entry),
            AbandonOutcome::Raced => report.raced += 1,
            AbandonOutcome::Skipped => {}
        }
    }
    report
}

enum AbandonOutcome {
    Abandoned(AbandonedShipState),
    Raced,
    Skipped,
}

/// Abandon a single PR's ship-state under its per-PR lock, re-verifying at lock
/// time that it is still in flight AND still a strong (`QueueStale`) orphan
/// against a **fresh** queue read — never the sweep-wide snapshot. This defends
/// against a verdict, a competing writer, or a re-ship's worker starting between
/// the snapshot classification and acquiring the lock: a heartbeat that landed
/// during the sweep now shows the owning job as live, so the state is left alone.
fn abandon_one(
    store: &ShipStateStore,
    state_dir: &Path,
    stale_after: Duration,
    repo: &str,
    pr: u64,
    now: DateTime<Utc>,
) -> AbandonOutcome {
    store
        .with_pr_state_scoped_locked(repo, pr, |current| {
            let Some(state) = current.as_mut() else {
                return Ok(AbandonOutcome::Skipped);
            };
            // A verdict (or a prior abandon) may have landed since the snapshot.
            if ship_terminal_verdict(state).is_some() {
                return Ok(AbandonOutcome::Raced);
            }
            // Re-derive liveness from a fresh queue read under the lock so the
            // destructive decision never trusts the (possibly stale) sweep
            // snapshot.
            let outcome = with_liveness_context(state_dir, stale_after, |liveness| {
                match liveness.classify(state, now) {
                    Some(orphan) if orphan.evidence == OrphanEvidence::QueueStale => {
                        let job_id = liveness
                            .match_job(state, now)
                            .job()
                            .map(|job| job.id.clone());
                        let entry = AbandonedShipState {
                            pr: state.pr,
                            repo: state.repo.clone(),
                            evidence: orphan.evidence.as_str().to_owned(),
                            stalled_minutes: orphan.stalled_minutes,
                        };
                        state.mark_abandoned(AbandonRecord {
                            reason: format!(
                                "orphaned: {} ({}m idle); re-ship required",
                                orphan.evidence.cause(),
                                orphan.stalled_minutes
                            ),
                            evidence: orphan.evidence.as_str().to_owned(),
                            stalled_minutes: orphan.stalled_minutes,
                            job_id,
                            abandoned_at: now,
                        });
                        AbandonOutcome::Abandoned(entry)
                    }
                    // No longer a strong orphan under the lock — leave it alone.
                    _ => AbandonOutcome::Raced,
                }
            });
            Ok(outcome)
        })
        .unwrap_or(AbandonOutcome::Skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use toml::Table;

    use crate::config::LocalOverlaySource;
    use crate::job::{Job, Priority, ValidationMode};
    use crate::queue::Queue;
    use crate::queue_request::{QueueRequestStore, QueuedExecutionEnvelope};
    use crate::ship::ShipExecutionRequest;
    use crate::ship_state::ShipState;

    const REPO: &str = "danielraffel/pulp";

    fn config(toml: &str) -> LoadedConfig {
        LoadedConfig {
            data: toml.parse::<Table>().expect("toml"),
            global_dir: std::path::PathBuf::from("/tmp/global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn on() -> LoadedConfig {
        config("[ship_state]\nauto_resume = true\n")
    }

    /// Write an in-flight ship-state (empty evidence => terminal verdict is
    /// `None`) idle for `idle_minutes`.
    fn write_in_flight_state(state_dir: &Path, pr: u64, idle_minutes: i64) {
        let store = ShipStateStore::new(state_dir.join("ship")).expect("store");
        let mut state = ShipState::new(pr, REPO, format!("shipyard-pr-{pr}"), "main", "sha", "pol");
        state.updated_at = Utc::now() - Duration::minutes(idle_minutes);
        store.save(&state).expect("save state");
    }

    fn base_job() -> Job {
        Job::create(
            "sha",
            "shipyard-pr-x",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        )
    }

    /// Persist `job` to the durable queue and map it to `{pr, REPO}` via a saved
    /// ship envelope, so `with_liveness_context` resolves the owner.
    fn enqueue_owner(state_dir: &Path, pr: u64, job: Job) {
        let request = ShipExecutionRequest {
            pr,
            repo: REPO.to_owned(),
            branch: format!("shipyard-pr-{pr}"),
            base_branch: "main".to_owned(),
            sha: "sha".to_owned(),
            commit_subject: String::new(),
            pr_url: None,
            pr_title: None,
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            advisory_targets: std::collections::BTreeSet::new(),
            adopt_head: false,
            pr_snapshot_file: None,
            targets: Vec::new(),
        };
        QueueRequestStore::new(state_dir)
            .expect("request store")
            .save(&QueuedExecutionEnvelope::from_ship_request(
                job.id.clone(),
                "/work",
                &request,
            ))
            .expect("save envelope");
        Queue::new(state_dir)
            .expect("queue")
            .enqueue(job)
            .expect("enqueue");
    }

    fn stale_running() -> Job {
        let mut job = base_job().start().expect("start");
        // Liveness anchor 1000s ago is well past the ~180s reaper threshold.
        job.started_at = Some(Utc::now() - Duration::seconds(1000));
        job
    }

    fn is_abandoned(state_dir: &Path, pr: u64) -> bool {
        ShipStateStore::new(state_dir.join("ship"))
            .expect("store")
            .get(pr)
            .expect("state")
            .is_abandoned()
    }

    #[test]
    fn disabled_config_is_a_no_op_even_with_a_dead_worker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        write_in_flight_state(dir, 1, 5);
        enqueue_owner(dir, 1, stale_running());

        // Absent `[ship_state]` block => auto_resume off => nothing happens.
        let report = sweep_orphaned_ship_states(dir, &config(""), Utc::now());
        assert!(report.abandoned.is_empty());
        assert!(!is_abandoned(dir, 1), "a disabled sweep must never mutate");
    }

    #[test]
    fn queue_stale_orphan_is_abandoned_and_becomes_terminal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        write_in_flight_state(dir, 1, 7);
        enqueue_owner(dir, 1, stale_running());

        let report = sweep_orphaned_ship_states(dir, &on(), Utc::now());

        assert_eq!(report.abandoned.len(), 1);
        let entry = &report.abandoned[0];
        assert_eq!(entry.pr, 1);
        assert_eq!(entry.repo, REPO);
        assert_eq!(entry.evidence, "queue_stale");

        let state = ShipStateStore::new(dir.join("ship"))
            .expect("store")
            .get(1)
            .expect("state");
        assert!(state.is_abandoned());
        let record = state.abandoned.as_ref().expect("record");
        assert_eq!(record.evidence, "queue_stale");
        assert!(record.job_id.is_some(), "records the dead owning job id");
        // The state is now terminally failed so the wait/auto-merge path stops.
        assert_eq!(crate::watch::ship_terminal_verdict(&state), Some(false));
    }

    #[test]
    fn abandonment_is_idempotent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        write_in_flight_state(dir, 1, 7);
        enqueue_owner(dir, 1, stale_running());

        let first = sweep_orphaned_ship_states(dir, &on(), Utc::now());
        assert_eq!(first.abandoned.len(), 1);
        // A second pass sees a terminal (abandoned) state => classify None.
        let second = sweep_orphaned_ship_states(dir, &on(), Utc::now());
        assert!(
            second.abandoned.is_empty(),
            "never re-abandons a terminal state"
        );
    }

    #[test]
    fn live_running_worker_is_never_abandoned() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        write_in_flight_state(dir, 1, 90); // very idle state...
        enqueue_owner(dir, 1, base_job().start().expect("start")); // ...but a fresh worker

        let report = sweep_orphaned_ship_states(dir, &on(), Utc::now());
        assert!(report.abandoned.is_empty(), "a heartbeating worker owns it");
        assert!(!is_abandoned(dir, 1));
    }

    #[test]
    fn pending_worker_is_never_abandoned() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        write_in_flight_state(dir, 1, 90);
        enqueue_owner(dir, 1, base_job()); // Pending (not started)

        let report = sweep_orphaned_ship_states(dir, &on(), Utc::now());
        assert!(report.abandoned.is_empty(), "a queued job is about to run");
        assert!(!is_abandoned(dir, 1));
    }

    #[test]
    fn terminal_job_orphan_is_not_abandoned_only_queue_stale_acts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        write_in_flight_state(dir, 1, 90);
        // Job finished but the ship-state never finalized => QueueTerminal, which
        // could be a success mid-write; it stays report-only, never abandoned.
        let terminal = base_job().start().expect("start").complete().expect("done");
        enqueue_owner(dir, 1, terminal);

        let report = sweep_orphaned_ship_states(dir, &on(), Utc::now());
        assert!(
            report.abandoned.is_empty(),
            "QueueTerminal evidence is report-only, never abandoned"
        );
        assert!(!is_abandoned(dir, 1));
    }

    #[test]
    fn queue_absent_stale_state_is_not_abandoned_fail_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        // A very stale in-flight state but NO queue at all => weakest evidence
        // (time-only); the destructive action must fail closed and never fire.
        write_in_flight_state(dir, 1, 600);

        let report = sweep_orphaned_ship_states(dir, &on(), Utc::now());
        assert!(
            report.abandoned.is_empty(),
            "no queue => no provable dead worker => never abandon"
        );
        assert!(!is_abandoned(dir, 1));
    }

    #[test]
    fn evidence_terminal_state_is_left_alone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        let store = ShipStateStore::new(dir.join("ship")).expect("store");
        let mut state = ShipState::new(1, REPO, "shipyard-pr-1", "main", "sha", "pol");
        // A real terminal verdict (all evidence present) — not an orphan.
        state.update_evidence("macos", "pass");
        state.updated_at = Utc::now() - Duration::minutes(90);
        store.save(&state).expect("save");
        enqueue_owner(dir, 1, stale_running());

        let report = sweep_orphaned_ship_states(dir, &on(), Utc::now());
        assert!(
            report.abandoned.is_empty(),
            "a state with a verdict is terminal"
        );
    }

    #[test]
    fn abandon_one_abandons_a_provably_dead_worker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        write_in_flight_state(dir, 1, 7);
        enqueue_owner(dir, 1, stale_running());
        let store = ShipStateStore::new(dir.join("ship")).expect("store");

        let outcome = abandon_one(&store, dir, Duration::minutes(45), REPO, 1, Utc::now());
        assert!(matches!(outcome, AbandonOutcome::Abandoned(_)));
        assert!(is_abandoned(dir, 1));
    }

    #[test]
    fn abandon_one_reads_live_queue_and_spares_a_revived_worker() {
        // The candidate may have looked stale in the sweep snapshot, but
        // abandon_one re-reads the queue live under the lock: a worker that is
        // heartbeating by the time the lock is held must never be abandoned.
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        write_in_flight_state(dir, 1, 90);
        enqueue_owner(dir, 1, base_job().start().expect("start")); // live on disk
        let store = ShipStateStore::new(dir.join("ship")).expect("store");

        let outcome = abandon_one(&store, dir, Duration::minutes(45), REPO, 1, Utc::now());
        assert!(matches!(outcome, AbandonOutcome::Raced));
        assert!(!is_abandoned(dir, 1));
    }
}
