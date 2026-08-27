//! Read-only liveness classification for in-flight ship states.
//!
//! A `shipyard ship` worker that dies mid-validation (host reboot from a jetsam
//! kill, daemon crash, `cmux` relaunch) leaves its durable `ShipState` frozen in
//! an in-flight verdict forever: `ship_terminal_verdict` stays `None`, so
//! auto-merge reports `InFlight` and never merges. The queue's killed-worker
//! reaper recovers the sibling `Job`, but the ship-state store has no orphan
//! lifecycle, so the stall is otherwise invisible.
//!
//! This module classifies that condition for diagnostics (`ship-state list`,
//! `status`). It is **strictly read-only** — it never mutates the queue or the
//! ship-state, and can never affect merge readiness (a flagged state is
//! in-flight, which auto-merge already refuses).
//!
//! The signal is **source-aware**. The durable ship-state carries no fresh
//! heartbeat (it is written once before a leg runs and again only at
//! completion) and no owning job id, so orphanhood is established by
//! cross-referencing the queue snapshot:
//!
//! - [`OrphanEvidence::QueueStale`] — a matching *running* job exists but its
//!   heartbeat is dead past the reaper threshold. The worker is provably gone;
//!   flagged immediately, no time gate.
//! - [`OrphanEvidence::QueueTerminal`] — a matching job is already terminal
//!   (completed/reaped) while the ship-state never reached a verdict. The worker
//!   ended without finalizing; flagged immediately.
//! - [`OrphanEvidence::QueueAbsent`] — the queue was consulted but no matching
//!   job was found. The ship-state stores no job id, so absence is *inferred*,
//!   not proof; only flagged once the state is also time-stale.
//! - [`OrphanEvidence::TimeFallback`] — the queue could not be consulted; the
//!   pure `updated_at` staleness heuristic is used, also time-gated.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::evidence::canonical_repository;

use crate::config::LoadedConfig;
use crate::job::{DEFAULT_RUNNING_JOB_STALE_SECONDS, Job, JobStatus};
use crate::queue::Queue;
use crate::queue_request::{QueueRequestStore, QueuedExecutionRequest};
use crate::ship_state::{ShipState, ShipStateStore};
use crate::watch::ship_terminal_verdict;

/// Default `updated_at` staleness (minutes) before a time-gated in-flight
/// ship-state is reported orphaned. Chosen well above a normally-paced
/// validation leg so the weak (absence/time) signals rarely false-positive.
pub const DEFAULT_ORPHAN_STALE_MINUTES: i64 = 45;

/// Upper bound (minutes ≈ one year) on the configured threshold, so a
/// misconfigured huge `orphan_stale_minutes` cannot overflow `Duration::minutes`
/// (which panics out of range) and effectively never flags anything anyway.
const MAX_ORPHAN_STALE_MINUTES: i64 = 365 * 24 * 60;

/// How an in-flight ship-state's suspected-orphan status was established, from
/// strongest (provable dead worker) to weakest (pure time heuristic).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanEvidence {
    /// Matching running job whose heartbeat is dead past the reaper threshold.
    QueueStale,
    /// Matching job already terminal while the ship-state never finalized.
    QueueTerminal,
    /// Queue consulted, no matching job found (absence is inferred, time-gated).
    QueueAbsent,
    /// Queue unavailable; `updated_at` staleness heuristic only (time-gated).
    TimeFallback,
}

impl OrphanEvidence {
    /// Machine-stable label used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QueueStale => "queue_stale",
            Self::QueueTerminal => "queue_terminal",
            Self::QueueAbsent => "queue_absent",
            Self::TimeFallback => "time_fallback",
        }
    }

    /// A short operator-facing cause phrase.
    #[must_use]
    pub fn cause(self) -> &'static str {
        match self {
            Self::QueueStale => "owning worker heartbeat is dead",
            Self::QueueTerminal => "owning worker ended without finalizing",
            Self::QueueAbsent => "no owning worker in the queue",
            Self::TimeFallback => "no update and queue unavailable",
        }
    }
}

/// A ship-state judged (probably) orphaned, with the evidence that established
/// it and how long it has been stalled (minutes since `updated_at`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OrphanReport {
    /// How orphanhood was established (strongest to weakest signal).
    pub evidence: OrphanEvidence,
    /// Minutes since the ship-state's `updated_at` (how long it has been frozen).
    pub stalled_minutes: i64,
}

/// The heartbeat-staleness window the queue reaper uses to declare a worker
/// dead. Reused here so "dead worker" means the same thing in both places.
#[must_use]
pub fn default_heartbeat_stale_after() -> Duration {
    Duration::seconds(DEFAULT_RUNNING_JOB_STALE_SECONDS)
}

/// Read the configurable orphan staleness threshold from `[ship_state]`
/// (`orphan_stale_minutes`), defaulting to [`DEFAULT_ORPHAN_STALE_MINUTES`].
/// Clamped to at least one minute so a misconfigured `0` cannot flag every
/// in-flight state instantly.
#[must_use]
pub fn orphan_stale_after(config: &LoadedConfig) -> Duration {
    let minutes = config
        .get("ship_state")
        .and_then(|value| value.clone().try_into().ok())
        .and_then(|cfg: ShipStateConfig| cfg.orphan_stale_minutes)
        .unwrap_or(DEFAULT_ORPHAN_STALE_MINUTES)
        .clamp(1, MAX_ORPHAN_STALE_MINUTES);
    Duration::minutes(minutes)
}

/// Raw `[ship_state]` config sub-table. `#[serde(default)]` keeps the whole
/// block optional so absence yields the default threshold.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
struct ShipStateConfig {
    orphan_stale_minutes: Option<i64>,
    auto_resume: bool,
    queue_absent_recovery: bool,
    repo_paths: BTreeMap<String, PathBuf>,
}

/// Whether exact queue-absence recovery is enabled. Missing or malformed
/// configuration is always disabled.
#[must_use]
pub fn queue_absent_recovery_enabled(config: &LoadedConfig) -> bool {
    config
        .get("ship_state")
        .and_then(|value| value.clone().try_into().ok())
        .is_some_and(|cfg: ShipStateConfig| cfg.queue_absent_recovery)
}

/// Resolve the explicitly registered checkout for a repository. Recovery
/// never guesses a checkout from the daemon's current directory.
#[must_use]
pub fn queue_absent_repo_path(config: &LoadedConfig, repo: &str) -> Option<PathBuf> {
    let canonical = canonical_repository(repo);
    config
        .get("ship_state")
        .and_then(|value| value.clone().try_into().ok())
        .and_then(|cfg: ShipStateConfig| {
            cfg.repo_paths.into_iter().find_map(|(registered, path)| {
                (canonical_repository(&registered) == canonical).then_some(path)
            })
        })
}

/// Whether the daemon's opt-in orphan auto-resume sweep is enabled
/// (`[ship_state] auto_resume`, default `false`). When off, the daemon never
/// opens the queue for a resume pass and never mutates a ship-state.
#[must_use]
pub fn auto_resume_enabled(config: &LoadedConfig) -> bool {
    config
        .get("ship_state")
        .and_then(|value| value.clone().try_into().ok())
        .is_some_and(|cfg: ShipStateConfig| cfg.auto_resume)
}

/// The queue's verdict on the job (if any) owning a ship-state's `{repo, pr}`.
///
/// The owning `Job` is carried for the `Pending`/`Running`/`Terminal` cases so
/// callers that need to *act* on it (a future retry or resume) have the exact
/// identity, not just a boolean. The report path only reads the variant.
#[derive(Clone, Copy, Debug)]
pub enum QueueMatch<'a> {
    /// The queue snapshot could not be read (treated as no evidence).
    Unavailable,
    /// The queue was consulted; no job owns this ship-state. Absence is inferred
    /// (the ship-state stores no job id), not proof.
    Absent,
    /// A pending (queued, not yet started) job owns it — a worker will start it.
    Pending(&'a Job),
    /// A running job owns it. `stale` is true when its heartbeat is dead past the
    /// reaper threshold — a provably gone worker.
    Running {
        /// The owning running job.
        job: &'a Job,
        /// Whether the worker's heartbeat is dead past the reaper threshold.
        stale: bool,
    },
    /// A terminal (completed/cancelled) job owns it.
    Terminal(&'a Job),
}

impl<'a> QueueMatch<'a> {
    /// The owning job, when one was matched.
    #[must_use]
    pub fn job(&self) -> Option<&'a Job> {
        match *self {
            Self::Pending(job) | Self::Terminal(job) | Self::Running { job, .. } => Some(job),
            Self::Unavailable | Self::Absent => None,
        }
    }
}

/// Resolve the queue's verdict on the job owning `state`'s `{repo, pr}`.
///
/// `jobs` is a single queue snapshot (`None` ⇒ [`QueueMatch::Unavailable`]).
/// `pr_repo_of` maps a job to its ship request's `(pr, repo)` — production loads
/// the [`QueueRequestStore`] envelope; tests inject a closure. Among multiple
/// matches the *most-alive* verdict wins (live running, then stale running, then
/// pending, then terminal), so a stale sibling never masks a live worker
/// regardless of the snapshot order. Read-only.
#[must_use]
pub fn match_ship_job<'a>(
    state: &ShipState,
    jobs: Option<&'a [Job]>,
    pr_repo_of: impl Fn(&Job) -> Option<(u64, String)>,
    now: DateTime<Utc>,
    heartbeat_stale_after: Duration,
) -> QueueMatch<'a> {
    let Some(jobs) = jobs else {
        return QueueMatch::Unavailable;
    };
    let (mut live_running, mut dead_running, mut pending, mut terminal) = (None, None, None, None);
    for job in jobs {
        let Some((pr, repo)) = pr_repo_of(job) else {
            continue;
        };
        if pr != state.pr || canonical_repository(&repo) != canonical_repository(&state.repo) {
            continue;
        }
        match job.status {
            JobStatus::Running if job.is_stale_running(now, heartbeat_stale_after) => {
                dead_running.get_or_insert(job);
            }
            JobStatus::Running => {
                live_running.get_or_insert(job);
            }
            JobStatus::Pending => {
                pending.get_or_insert(job);
            }
            JobStatus::Completed | JobStatus::Cancelled => {
                terminal.get_or_insert(job);
            }
        }
    }
    if let Some(job) = live_running {
        QueueMatch::Running { job, stale: false }
    } else if let Some(job) = dead_running {
        QueueMatch::Running { job, stale: true }
    } else if let Some(job) = pending {
        QueueMatch::Pending(job)
    } else if let Some(job) = terminal {
        QueueMatch::Terminal(job)
    } else {
        QueueMatch::Absent
    }
}

/// Classify a single in-flight ship-state given the queue's verdict on its
/// owning job ([`match_ship_job`]).
///
/// Returns `None` when the state is terminal (not in flight), when a matching
/// worker is alive (running-fresh or pending), or when a weak (time-gated)
/// signal has not yet crossed `stale_after`. Never mutates anything.
#[must_use]
pub fn classify_orphan(
    state: &ShipState,
    queue_match: &QueueMatch<'_>,
    now: DateTime<Utc>,
    stale_after: Duration,
) -> Option<OrphanReport> {
    // Only in-flight states can be orphaned; a terminal verdict is handled by
    // the normal merge/archive path, not here.
    if ship_terminal_verdict(state).is_some() {
        return None;
    }
    let stalled_minutes = now.signed_duration_since(state.updated_at).num_minutes();

    let evidence = match queue_match {
        // A provably dead worker (heartbeat past the reaper threshold).
        QueueMatch::Running { stale: true, .. } => OrphanEvidence::QueueStale,
        // A live worker (heartbeating) or a queued job about to start is not an
        // orphan, however old the durable ship-state's `updated_at` looks.
        QueueMatch::Running { stale: false, .. } | QueueMatch::Pending(_) => return None,
        // Terminal while the ship-state never finalized — abandoned.
        QueueMatch::Terminal(_) => OrphanEvidence::QueueTerminal,
        QueueMatch::Absent => OrphanEvidence::QueueAbsent,
        QueueMatch::Unavailable => OrphanEvidence::TimeFallback,
    };

    // Strong signals (a provably dead or terminal owning job) flag immediately.
    // Weak signals (inferred absence / no queue) require the time threshold so a
    // just-created or briefly-untracked state is not mislabeled.
    let time_gated = matches!(
        evidence,
        OrphanEvidence::QueueAbsent | OrphanEvidence::TimeFallback
    );
    if time_gated && stalled_minutes < stale_after.num_minutes() {
        return None;
    }

    Some(OrphanReport {
        evidence,
        stalled_minutes: stalled_minutes.max(0),
    })
}

/// Bundles a queue snapshot + request store so callers can resolve a ship-state's
/// [`QueueMatch`] and orphan classification without re-plumbing the inputs.
pub struct LivenessContext<'a> {
    jobs: Option<&'a [Job]>,
    request_store: Option<&'a QueueRequestStore>,
    stale_after: Duration,
    heartbeat_stale_after: Duration,
}

impl<'a> LivenessContext<'a> {
    /// Full context backed by a queue snapshot + request store.
    #[must_use]
    pub fn from_queue(
        jobs: &'a [Job],
        request_store: &'a QueueRequestStore,
        stale_after: Duration,
    ) -> Self {
        Self {
            jobs: Some(jobs),
            request_store: Some(request_store),
            stale_after,
            heartbeat_stale_after: default_heartbeat_stale_after(),
        }
    }

    /// Fallback context with no queue evidence — pure `updated_at` staleness.
    #[must_use]
    pub fn time_only(stale_after: Duration) -> Self {
        Self {
            jobs: None,
            request_store: None,
            stale_after,
            heartbeat_stale_after: default_heartbeat_stale_after(),
        }
    }

    /// Resolve the queue verdict for a ship-state. Public so a future retry/resume
    /// can act on the owning job via the returned [`QueueMatch`]. Read-only.
    #[must_use]
    pub fn match_job(&self, state: &ShipState, now: DateTime<Utc>) -> QueueMatch<'a> {
        let store = self.request_store;
        match_ship_job(
            state,
            self.jobs,
            |job| store.and_then(|store| ship_pr_repo(store, &job.id)),
            now,
            self.heartbeat_stale_after,
        )
    }

    /// Classify one ship-state for reporting. Read-only.
    #[must_use]
    pub fn classify(&self, state: &ShipState, now: DateTime<Utc>) -> Option<OrphanReport> {
        classify_orphan(state, &self.match_job(state, now), now, self.stale_after)
    }
}

/// Classify every active ship-state in `store` against `liveness`, returning the
/// orphaned ones as `(repo, pr, report)` tuples for rendering. Read-only — keeps the
/// store/snapshot mechanics out of the command layer.
#[must_use]
pub fn collect_orphans(
    store: &ShipStateStore,
    liveness: &LivenessContext<'_>,
    now: DateTime<Utc>,
) -> Vec<(String, u64, OrphanReport)> {
    store
        .list_active()
        .iter()
        .filter_map(|state| {
            liveness
                .classify(state, now)
                .map(|report| (state.repo.clone(), state.pr, report))
        })
        .collect()
}

/// Build a read-only [`LivenessContext`] from a single queue snapshot rooted at
/// `state_dir` and hand it to `f`. Best-effort: if the queue is absent or cannot
/// be read, falls back to a time-only context so a diagnostic never fails just
/// because the queue is missing.
///
/// Strictly read-only: it opens the queue/request stores only when the queue
/// directory already exists, so running a diagnostic in a fresh directory
/// creates nothing (the store constructors would otherwise `create_dir_all`).
/// The snapshot read (`get_all`) takes the queue's state lock — the same lock
/// mutations use — and never writes.
pub fn with_liveness_context<R>(
    state_dir: &Path,
    stale_after: Duration,
    f: impl FnOnce(&LivenessContext<'_>) -> R,
) -> R {
    // The durable `Queue` lives directly in `state_dir`; `QueueRequestStore`
    // appends `queue/requests` internally. Only touch them when queue.json
    // already exists so a diagnostic never materializes state.
    let queue_file = Queue::queue_file_at(state_dir);
    let request_dir = state_dir.join("queue").join("requests");
    let (jobs, request_store) = if queue_file.is_file() {
        let jobs = match Queue::new(state_dir) {
            Ok(mut queue) => queue.get_all().ok(),
            Err(_) => None,
        };
        // Only open the request store when it already exists — otherwise its
        // constructor would `create_dir_all`. Absent means no ship jobs were ever
        // enqueued, so there is nothing to map and time-fallback is correct.
        let request_store = if request_dir.is_dir() {
            QueueRequestStore::new(state_dir).ok()
        } else {
            None
        };
        (jobs, request_store)
    } else {
        (None, None)
    };
    let context = match (jobs.as_deref(), request_store.as_ref()) {
        (Some(jobs), Some(store)) => LivenessContext::from_queue(jobs, store, stale_after),
        _ => LivenessContext::time_only(stale_after),
    };
    f(&context)
}

/// Load a job's ship request `(pr, repo)` from the request store, or `None` if
/// the envelope is missing/unreadable or the job is not a ship request.
fn ship_pr_repo(store: &QueueRequestStore, job_id: &str) -> Option<(u64, String)> {
    let envelope = store.load(job_id).ok().flatten()?;
    match envelope.request {
        QueuedExecutionRequest::Ship(request) => Some((request.pr, request.repo)),
        QueuedExecutionRequest::Run(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, Priority, ValidationMode};
    use crate::ship::ShipExecutionRequest;
    use crate::ship_state::ShipState;

    fn in_flight_state(pr: u64) -> ShipState {
        // Empty evidence => `ship_terminal_verdict` is `None` => in flight.
        ShipState::new(
            pr,
            "danielraffel/pulp",
            format!("shipyard-pr-{pr}"),
            "main",
            "0000000000000000000000000000000000000000",
            "policy0001",
        )
    }

    fn terminal_state(pr: u64) -> ShipState {
        let mut state = in_flight_state(pr);
        state.update_evidence("macos", "pass");
        state
    }

    fn running_job(started_secs_ago: i64) -> Job {
        let mut job = Job::create(
            "sha",
            "shipyard-pr-1",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        )
        .start()
        .expect("start");
        job.started_at = Some(Utc::now() - Duration::seconds(started_secs_ago));
        job
    }

    const STALE: Duration = Duration::minutes(45);

    #[test]
    fn terminal_state_is_never_orphaned() {
        assert!(
            classify_orphan(&terminal_state(1), &QueueMatch::Absent, Utc::now(), STALE).is_none()
        );
    }

    #[test]
    fn live_running_job_is_not_orphaned_even_when_state_looks_old() {
        let mut state = in_flight_state(1);
        state.updated_at = Utc::now() - Duration::hours(3);
        let job = running_job(5); // fresh heartbeat anchor
        let m = QueueMatch::Running {
            job: &job,
            stale: false,
        };
        assert!(classify_orphan(&state, &m, Utc::now(), STALE).is_none());
    }

    #[test]
    fn pending_job_is_not_orphaned() {
        // A queued-but-not-started ship is alive; the ship-state is saved before
        // the job is marked running, so it must not be flagged.
        let mut state = in_flight_state(1);
        state.updated_at = Utc::now() - Duration::hours(3);
        let job = running_job(5); // any job; only the variant matters here
        assert!(classify_orphan(&state, &QueueMatch::Pending(&job), Utc::now(), STALE).is_none());
    }

    #[test]
    fn stale_running_job_flags_queue_stale_immediately() {
        // State only 2m old, but the matching worker's heartbeat is dead.
        let mut state = in_flight_state(1);
        state.updated_at = Utc::now() - Duration::minutes(2);
        let job = running_job(1000);
        let m = QueueMatch::Running {
            job: &job,
            stale: true,
        };
        let report = classify_orphan(&state, &m, Utc::now(), STALE).expect("stale worker orphaned");
        assert_eq!(report.evidence, OrphanEvidence::QueueStale);
    }

    #[test]
    fn terminal_job_with_in_flight_state_flags_queue_terminal_immediately() {
        let mut state = in_flight_state(1);
        state.updated_at = Utc::now() - Duration::minutes(1);
        let job = running_job(10).complete().expect("complete");
        let report = classify_orphan(&state, &QueueMatch::Terminal(&job), Utc::now(), STALE)
            .expect("abandoned state orphaned");
        assert_eq!(report.evidence, OrphanEvidence::QueueTerminal);
    }

    #[test]
    fn absent_job_is_time_gated() {
        let mut fresh = in_flight_state(1);
        fresh.updated_at = Utc::now() - Duration::minutes(10);
        assert!(
            classify_orphan(&fresh, &QueueMatch::Absent, Utc::now(), STALE).is_none(),
            "fresh absent state must not flag"
        );

        let mut old = in_flight_state(1);
        old.updated_at = Utc::now() - Duration::minutes(60);
        let report = classify_orphan(&old, &QueueMatch::Absent, Utc::now(), STALE)
            .expect("stale absent state orphaned");
        assert_eq!(report.evidence, OrphanEvidence::QueueAbsent);
    }

    #[test]
    fn unavailable_queue_is_time_fallback() {
        let mut old = in_flight_state(1);
        old.updated_at = Utc::now() - Duration::minutes(60);
        let report = classify_orphan(&old, &QueueMatch::Unavailable, Utc::now(), STALE)
            .expect("time fallback orphaned");
        assert_eq!(report.evidence, OrphanEvidence::TimeFallback);
    }

    #[test]
    fn future_updated_at_never_panics_or_flags() {
        let mut skewed = in_flight_state(1);
        skewed.updated_at = Utc::now() + Duration::minutes(30); // clock skew
        assert!(classify_orphan(&skewed, &QueueMatch::Absent, Utc::now(), STALE).is_none());
    }

    #[test]
    fn match_ship_job_unavailable_when_no_snapshot() {
        let state = in_flight_state(7);
        let m = match_ship_job(
            &state,
            None,
            |_| Some((7, "danielraffel/pulp".to_owned())),
            Utc::now(),
            default_heartbeat_stale_after(),
        );
        assert!(matches!(m, QueueMatch::Unavailable));
    }

    #[test]
    fn match_ship_job_prefers_live_over_stale_regardless_of_order() {
        let state = in_flight_state(7);
        let dead = running_job(1000); // dead heartbeat
        let live = running_job(5); // fresh
        // Dead listed first: the live worker must still win.
        let jobs = vec![dead.clone(), live.clone()];
        let map = |_: &Job| Some((7, "danielraffel/pulp".to_owned()));
        let m = match_ship_job(
            &state,
            Some(&jobs),
            map,
            Utc::now(),
            default_heartbeat_stale_after(),
        );
        match m {
            QueueMatch::Running { job, stale: false } => assert_eq!(job.id, live.id),
            other => panic!("expected live running, got {other:?}"),
        }
    }

    #[test]
    fn match_ship_job_absent_on_repo_mismatch() {
        let state = in_flight_state(7);
        let job = running_job(5);
        let jobs = vec![job];
        let map = |_: &Job| Some((7, "someone/else".to_owned()));
        let m = match_ship_job(
            &state,
            Some(&jobs),
            map,
            Utc::now(),
            default_heartbeat_stale_after(),
        );
        assert!(matches!(m, QueueMatch::Absent));
    }

    fn loaded_config(toml: &str) -> LoadedConfig {
        use crate::config::LocalOverlaySource;
        LoadedConfig {
            data: toml.parse::<toml::Table>().expect("toml"),
            global_dir: std::path::PathBuf::from("/tmp/global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    #[test]
    fn orphan_stale_after_defaults_and_reads_config() {
        assert_eq!(
            orphan_stale_after(&loaded_config("")),
            Duration::minutes(DEFAULT_ORPHAN_STALE_MINUTES)
        );
        assert_eq!(
            orphan_stale_after(&loaded_config("[ship_state]\norphan_stale_minutes = 120\n")),
            Duration::minutes(120)
        );
        // A misconfigured `0` is clamped up to one minute, never zero.
        assert_eq!(
            orphan_stale_after(&loaded_config("[ship_state]\norphan_stale_minutes = 0\n")),
            Duration::minutes(1)
        );
        // A huge value is clamped to the upper bound — never a `Duration` overflow panic.
        assert_eq!(
            orphan_stale_after(&loaded_config(
                "[ship_state]\norphan_stale_minutes = 9223372036854775807\n"
            )),
            Duration::minutes(MAX_ORPHAN_STALE_MINUTES)
        );
    }

    #[test]
    fn auto_resume_defaults_off_and_reads_config() {
        assert!(!auto_resume_enabled(&loaded_config("")), "absent => off");
        assert!(
            !auto_resume_enabled(&loaded_config("[ship_state]\norphan_stale_minutes = 30\n")),
            "unrelated key => still off"
        );
        assert!(auto_resume_enabled(&loaded_config(
            "[ship_state]\nauto_resume = true\n"
        )));
        assert!(!auto_resume_enabled(&loaded_config(
            "[ship_state]\nauto_resume = false\n"
        )));
        // A wrong-typed value must not enable the sweep (fail safe to off).
        assert!(!auto_resume_enabled(&loaded_config(
            "[ship_state]\nauto_resume = \"yes\"\n"
        )));
    }

    /// End-to-end through a real `QueueRequestStore`: the `{pr, repo}` mapping is
    /// loaded from a saved ship envelope, matched to a stale running job, and
    /// classified `QueueStale`. Exercises `ship_pr_repo` + `match_ship_job` +
    /// `classify_orphan` together via `LivenessContext::from_queue`.
    #[test]
    fn from_queue_context_classifies_queue_stale_via_request_store() {
        use crate::queue_request::{QueueRequestStore, QueuedExecutionEnvelope};

        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("request store");
        let job = running_job(1000); // liveness anchor 1000s ago => stale
        let request = ShipExecutionRequest {
            pr: 1,
            repo: "DanielRaffel/Pulp".to_owned(),
            branch: "shipyard-pr-1".to_owned(),
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
            metadata_authority_receipt: None,
            targets: Vec::new(),
        };
        store
            .save(&QueuedExecutionEnvelope::from_ship_request(
                job.id.clone(),
                "/work",
                &request,
            ))
            .expect("save envelope");

        let jobs = vec![job];
        let context = LivenessContext::from_queue(&jobs, &store, Duration::minutes(45));
        let mut state = in_flight_state(1);
        state.updated_at = Utc::now() - Duration::minutes(2);

        let report = context
            .classify(&state, Utc::now())
            .expect("stale worker orphaned");
        assert_eq!(report.evidence, OrphanEvidence::QueueStale);

        // A different PR with no envelope match falls back to time-gating.
        let mut other = in_flight_state(999);
        other.updated_at = Utc::now() - Duration::minutes(2);
        assert!(
            context.classify(&other, Utc::now()).is_none(),
            "fresh unmatched PR must not flag"
        );
    }
}
