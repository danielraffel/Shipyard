//! Self-hosted runner watchdog: pure detection logic shared by the CLI handler.
//!
//! Ports the Pulp planning prototype at
//! `pulp-planning/scripts/runner-watchdog.sh` (commit c719482) into typed,
//! shell-free Rust helpers. The CLI side (`src/app/runner_cmd.rs`) is the only
//! place that shells out to `gh` and inspects the local process table; this
//! module just classifies inputs and produces report shapes.
//!
//! ## Symptoms tracked
//!
//! 1. **Runner reachability** — runner ID returns a payload with `status =
//!    "online"`. Otherwise `OFFLINE`.
//! 2. **Orphaned busy state** — runner API reports `busy = true` but no
//!    `Runner.Worker` process owned by this runner is visible in `ps`. The
//!    status commonly clears in 1-5 min; auto-fix is opt-in.
//! 3. **Hung Worker** — a `Runner.Worker` process has been running longer than
//!    `max_job_min` minutes.
//! 4. **Stale queued runs** — queued GitHub Actions runs older than
//!    `max_queue_age_hours`. With `--fix` the CLI cancels them.
//!
//! ## Why no auto-actions by default
//!
//! Killing a Worker process can corrupt in-flight artifacts; cancelling a
//! queued run can lose CI evidence the user is waiting on. `cleanup --fix`
//! and `--force-kill` are explicit opt-ins, matching the prototype's
//! convention.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::cloud::QueuedRun;

/// Defaults for the watchdog, mirroring `[runner.watchdog]` in
/// `.shipyard/config.toml`.
pub const DEFAULT_MAX_JOB_MIN: i64 = 90;
/// Default stale-queue age before a queued run is flagged.
pub const DEFAULT_MAX_QUEUE_AGE_HOURS: i64 = 2;
/// Default watch-mode polling interval in seconds.
pub const DEFAULT_WATCH_INTERVAL_SECONDS: u64 = 300;
/// Default max age (minutes) for an `in_progress` run before the stale-run
/// reaper treats it as hung. ~5h — well past any healthy run, so an in-flight
/// validation is never touched.
pub const DEFAULT_REAP_IN_PROGRESS_MAX_MIN: i64 = 300;
/// Default max age (minutes) for a `queued` run before the stale-run reaper
/// treats it as orphaned (~8h).
pub const DEFAULT_REAP_QUEUED_MAX_MIN: i64 = 480;

/// Resolved runner-watchdog thresholds for a single invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WatchdogThresholds {
    /// Warn when a `Runner.Worker` has been running longer than this many
    /// minutes.
    pub max_job_min: i64,
    /// Flag any queued workflow run created more than this many hours ago.
    pub max_queue_age_hours: i64,
    /// Polling interval for `runner watch`.
    pub watch_interval_seconds: u64,
    /// Whether `--fix` defaults to on.
    pub auto_fix: bool,
}

impl Default for WatchdogThresholds {
    fn default() -> Self {
        Self {
            max_job_min: DEFAULT_MAX_JOB_MIN,
            max_queue_age_hours: DEFAULT_MAX_QUEUE_AGE_HOURS,
            watch_interval_seconds: DEFAULT_WATCH_INTERVAL_SECONDS,
            auto_fix: false,
        }
    }
}

/// Overall runner-health classification.
///
/// Maps directly to the CLI exit codes: 0 healthy, 1 stuck, 2 unreachable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerHealth {
    /// Runner is online, no symptoms detected.
    Healthy,
    /// At least one symptom (orphaned busy / hung worker / stale queue)
    /// detected; runner is still reachable.
    Stuck,
    /// Runner is offline or the API call failed.
    Offline,
}

impl RunnerHealth {
    /// Exit code that should accompany this health verdict.
    #[must_use]
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::Stuck => 1,
            Self::Offline => 2,
        }
    }

    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Stuck => "stuck",
            Self::Offline => "offline",
        }
    }
}

/// One symptom found during a watchdog scan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Symptom {
    /// API reports busy but no `Runner.Worker` process is visible.
    OrphanedBusy,
    /// A `Runner.Worker` has been running longer than `max_job_min` minutes.
    HungWorker {
        /// Observed runtime in whole minutes.
        worker_age_min: i64,
        /// Threshold that was crossed.
        threshold_min: i64,
    },
    /// One or more queued runs are older than `max_queue_age_hours`.
    StaleQueuedRuns {
        /// Number of stale runs.
        count: usize,
    },
}

impl Symptom {
    /// Short tag used in human output.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::OrphanedBusy => "orphaned_busy",
            Self::HungWorker { .. } => "hung_worker",
            Self::StaleQueuedRuns { .. } => "stale_queued_runs",
        }
    }
}

/// Single stale queued-run row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StaleQueuedRun {
    /// GitHub Actions run ID.
    pub run_id: u64,
    /// Workflow display name.
    pub workflow: String,
    /// Head branch.
    pub branch: String,
    /// How long the run has been queued, in seconds.
    pub queued_for_secs: i64,
    /// Browser URL, when GitHub returned one.
    pub url: Option<String>,
}

/// Summary returned by [`assess_runner`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunnerReport {
    /// Final verdict for exit code.
    pub health: RunnerHealth,
    /// Concrete symptoms observed.
    pub symptoms: Vec<Symptom>,
    /// Stale queued runs (always present, even when not the reason for STUCK).
    pub stale_queued_runs: Vec<StaleQueuedRun>,
    /// Number of `Runner.Worker` processes observed locally.
    pub worker_count: usize,
    /// Whether the API reports the runner as busy.
    pub busy: bool,
    /// Reported runner status string (e.g. "online", "offline").
    pub status: String,
}

impl RunnerReport {
    /// Convenience: did the scan find anything actionable?
    #[must_use]
    pub fn is_clean(&self) -> bool {
        matches!(self.health, RunnerHealth::Healthy)
    }
}

/// Snapshot of the runner-side state, gathered by the caller (CLI shells out
/// to `gh` and `ps`). Kept as a plain struct so the pure analysis stays
/// trivially testable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerSnapshot {
    /// `status` field from `repos/<owner>/<repo>/actions/runners/<id>`.
    pub status: String,
    /// `busy` field from the same API.
    pub busy: bool,
    /// Number of locally-visible `Runner.Worker` processes.
    pub worker_count: usize,
    /// Age of the oldest `Runner.Worker`, in minutes.
    pub oldest_worker_age_min: Option<i64>,
}

/// Build a [`RunnerReport`] from a runner snapshot and a list of queued runs.
///
/// `now` is taken as a parameter so tests can pin the clock without touching
/// the system time.
#[must_use]
pub fn assess_runner(
    snapshot: &RunnerSnapshot,
    queued_runs: &[QueuedRun],
    thresholds: WatchdogThresholds,
    now: DateTime<Utc>,
) -> RunnerReport {
    if snapshot.status != "online" {
        return RunnerReport {
            health: RunnerHealth::Offline,
            symptoms: Vec::new(),
            stale_queued_runs: Vec::new(),
            worker_count: snapshot.worker_count,
            busy: snapshot.busy,
            status: snapshot.status.clone(),
        };
    }

    let mut symptoms = Vec::new();

    if snapshot.busy && snapshot.worker_count == 0 {
        symptoms.push(Symptom::OrphanedBusy);
    }

    if let Some(age) = snapshot.oldest_worker_age_min
        && age > thresholds.max_job_min
    {
        symptoms.push(Symptom::HungWorker {
            worker_age_min: age,
            threshold_min: thresholds.max_job_min,
        });
    }

    let stale_queued_runs =
        compute_stale_queued_runs(queued_runs, thresholds.max_queue_age_hours * 3_600, now);
    if !stale_queued_runs.is_empty() {
        symptoms.push(Symptom::StaleQueuedRuns {
            count: stale_queued_runs.len(),
        });
    }

    let health = if symptoms.is_empty() {
        RunnerHealth::Healthy
    } else {
        RunnerHealth::Stuck
    };

    RunnerReport {
        health,
        symptoms,
        stale_queued_runs,
        worker_count: snapshot.worker_count,
        busy: snapshot.busy,
        status: snapshot.status.clone(),
    }
}

/// Filter `queued_runs` down to entries older than `threshold_secs` relative
/// to `now`. Runs with unparseable timestamps are ignored, matching the
/// prototype's silent-skip behaviour.
#[must_use]
pub fn compute_stale_queued_runs(
    queued_runs: &[QueuedRun],
    threshold_secs: i64,
    now: DateTime<Utc>,
) -> Vec<StaleQueuedRun> {
    let mut out = Vec::new();
    for run in queued_runs {
        let Ok(created) = DateTime::parse_from_rfc3339(&run.created_at) else {
            continue;
        };
        let age_secs = (now - created.with_timezone(&Utc)).num_seconds();
        if age_secs < threshold_secs {
            continue;
        }
        out.push(StaleQueuedRun {
            run_id: run.database_id,
            workflow: run.workflow_name.clone(),
            branch: run.head_branch.clone(),
            queued_for_secs: age_secs,
            url: run.url.clone(),
        });
    }
    out
}

/// Why a workflow run was selected for reaping.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleRunKind {
    /// Run stuck `in_progress` past the in-progress max age (hung).
    HungInProgress,
    /// Run stuck `queued` past the queued max age (orphaned).
    OrphanedQueued,
}

impl StaleRunKind {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HungInProgress => "hung_in_progress",
            Self::OrphanedQueued => "orphaned_queued",
        }
    }
}

/// A single workflow run the stale-run reaper would cancel.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StaleRun {
    /// GitHub Actions run ID.
    pub run_id: u64,
    /// Workflow display name.
    pub workflow: String,
    /// Head branch.
    pub branch: String,
    /// Raw GitHub status (`queued` / `in_progress`).
    pub status: String,
    /// Why this run was flagged.
    pub kind: StaleRunKind,
    /// How long the run has existed, in seconds.
    pub age_secs: i64,
    /// Browser URL, when GitHub returned one.
    pub url: Option<String>,
}

/// Thresholds for the stale-run reaper, in minutes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReaperThresholds {
    /// Cancel `in_progress` runs older than this many minutes (hung).
    pub in_progress_max_min: i64,
    /// Cancel `queued` runs older than this many minutes (orphaned).
    pub queued_max_min: i64,
}

impl Default for ReaperThresholds {
    fn default() -> Self {
        Self {
            in_progress_max_min: DEFAULT_REAP_IN_PROGRESS_MAX_MIN,
            queued_max_min: DEFAULT_REAP_QUEUED_MAX_MIN,
        }
    }
}

/// Select workflow runs that are genuinely stale and should be cancelled.
///
/// `in_progress_runs` and `queued_runs` are the raw run lists from the GitHub
/// Actions API. A run is flagged only when its age strictly exceeds the
/// matching threshold — a healthy in-flight run, or one just under the
/// threshold, is always kept. Runs with an unparseable timestamp are silently
/// skipped, matching [`compute_stale_queued_runs`].
///
/// Age basis differs by kind:
///
/// * **`in_progress`** runs are aged from `run_started_at` (execution start),
///   so a run that sat in queue for hours and only just began executing is
///   *not* treated as hung. When GitHub did not report `run_started_at`, the
///   computation falls back to `created_at`.
/// * **`queued`** runs are aged from `created_at` — a queued run has not
///   started, so time-since-creation *is* its meaningful age.
///
/// `now` is a parameter so tests can pin the clock.
#[must_use]
pub fn compute_stale_runs(
    in_progress_runs: &[QueuedRun],
    queued_runs: &[QueuedRun],
    thresholds: ReaperThresholds,
    now: DateTime<Utc>,
) -> Vec<StaleRun> {
    let mut out = Vec::new();
    collect_stale(
        &mut out,
        in_progress_runs,
        thresholds.in_progress_max_min.max(0) * 60,
        StaleRunKind::HungInProgress,
        now,
    );
    collect_stale(
        &mut out,
        queued_runs,
        thresholds.queued_max_min.max(0) * 60,
        StaleRunKind::OrphanedQueued,
        now,
    );
    out
}

/// Pick the timestamp a run's age should be measured from.
///
/// For a [`StaleRunKind::HungInProgress`] run, "age" means how long it has
/// been *executing* — so prefer `run_started_at`, falling back to
/// `created_at` only when GitHub did not report a start time. For a
/// [`StaleRunKind::OrphanedQueued`] run, the run never started, so
/// `created_at` is the meaningful basis.
fn age_basis(run: &QueuedRun, kind: StaleRunKind) -> &str {
    match kind {
        StaleRunKind::HungInProgress => run
            .run_started_at
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or(&run.created_at),
        StaleRunKind::OrphanedQueued => &run.created_at,
    }
}

fn collect_stale(
    out: &mut Vec<StaleRun>,
    runs: &[QueuedRun],
    threshold_secs: i64,
    kind: StaleRunKind,
    now: DateTime<Utc>,
) {
    for run in runs {
        let Ok(started) = DateTime::parse_from_rfc3339(age_basis(run, kind)) else {
            continue;
        };
        let age_secs = (now - started.with_timezone(&Utc)).num_seconds();
        if age_secs <= threshold_secs {
            continue;
        }
        out.push(StaleRun {
            run_id: run.database_id,
            workflow: run.workflow_name.clone(),
            branch: run.head_branch.clone(),
            status: run.status.clone(),
            kind,
            age_secs,
            url: run.url.clone(),
        });
    }
}

/// Render a [`RunnerReport`] as a flat `BTreeMap<String, Value>` ready to be
/// dropped into `write_json_envelope`.
#[must_use]
pub fn report_to_json(report: &RunnerReport) -> BTreeMap<String, Value> {
    let mut data = BTreeMap::new();
    data.insert("health".to_owned(), Value::from(report.health.as_str()));
    data.insert("status".to_owned(), Value::from(report.status.clone()));
    data.insert("busy".to_owned(), Value::Bool(report.busy));
    data.insert("worker_count".to_owned(), Value::from(report.worker_count));
    data.insert(
        "symptoms".to_owned(),
        serde_json::to_value(&report.symptoms).expect("symptom serialization"),
    );
    data.insert(
        "stale_queued_runs".to_owned(),
        serde_json::to_value(&report.stale_queued_runs).expect("stale run serialization"),
    );
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn queued_run(id: u64, workflow: &str, branch: &str, created_at: &str) -> QueuedRun {
        QueuedRun {
            database_id: id,
            name: workflow.to_owned(),
            head_branch: branch.to_owned(),
            created_at: created_at.to_owned(),
            run_started_at: None,
            workflow_name: workflow.to_owned(),
            url: Some(format!("https://github.com/owner/repo/actions/runs/{id}")),
            path: ".github/workflows/ci.yml".to_owned(),
            status: "queued".to_owned(),
            conclusion: None,
        }
    }

    #[test]
    fn offline_runner_short_circuits_to_offline() {
        let snapshot = RunnerSnapshot {
            status: "offline".to_owned(),
            busy: false,
            worker_count: 0,
            oldest_worker_age_min: None,
        };
        let report = assess_runner(&snapshot, &[], WatchdogThresholds::default(), Utc::now());
        assert_eq!(report.health, RunnerHealth::Offline);
        assert_eq!(report.health.exit_code(), 2);
        assert!(report.symptoms.is_empty());
    }

    #[test]
    fn healthy_runner_has_no_symptoms() {
        let snapshot = RunnerSnapshot {
            status: "online".to_owned(),
            busy: false,
            worker_count: 0,
            oldest_worker_age_min: None,
        };
        let report = assess_runner(&snapshot, &[], WatchdogThresholds::default(), Utc::now());
        assert_eq!(report.health, RunnerHealth::Healthy);
        assert_eq!(report.health.exit_code(), 0);
    }

    #[test]
    fn busy_with_no_worker_flags_orphaned_busy() {
        let snapshot = RunnerSnapshot {
            status: "online".to_owned(),
            busy: true,
            worker_count: 0,
            oldest_worker_age_min: None,
        };
        let report = assess_runner(&snapshot, &[], WatchdogThresholds::default(), Utc::now());
        assert_eq!(report.health, RunnerHealth::Stuck);
        assert!(matches!(report.symptoms[0], Symptom::OrphanedBusy));
        assert_eq!(report.health.exit_code(), 1);
    }

    #[test]
    fn worker_running_longer_than_threshold_is_hung() {
        let snapshot = RunnerSnapshot {
            status: "online".to_owned(),
            busy: true,
            worker_count: 1,
            oldest_worker_age_min: Some(120),
        };
        let report = assess_runner(&snapshot, &[], WatchdogThresholds::default(), Utc::now());
        assert!(report.symptoms.iter().any(|s| matches!(
            s,
            Symptom::HungWorker {
                worker_age_min: 120,
                threshold_min: 90
            }
        )));
    }

    #[test]
    fn worker_at_threshold_is_not_flagged() {
        let snapshot = RunnerSnapshot {
            status: "online".to_owned(),
            busy: true,
            worker_count: 1,
            oldest_worker_age_min: Some(90),
        };
        let report = assess_runner(&snapshot, &[], WatchdogThresholds::default(), Utc::now());
        assert!(
            report
                .symptoms
                .iter()
                .all(|s| !matches!(s, Symptom::HungWorker { .. }))
        );
    }

    #[test]
    fn stale_queue_filter_uses_age_threshold() {
        let now = Utc::now();
        let runs = vec![
            queued_run(
                101,
                "CI",
                "agentB/81-window-only-capture",
                &(now - ChronoDuration::hours(5)).to_rfc3339(),
            ),
            queued_run(
                102,
                "CI",
                "feature/recent",
                &(now - ChronoDuration::minutes(15)).to_rfc3339(),
            ),
            queued_run(103, "CI", "feature/bad-ts", "not-a-timestamp"),
        ];

        let stale = compute_stale_queued_runs(&runs, 2 * 3_600, now);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].run_id, 101);
        assert!(stale[0].queued_for_secs >= 5 * 3_600);
        assert_eq!(stale[0].branch, "agentB/81-window-only-capture");
    }

    #[test]
    fn stale_queue_drives_stuck_verdict() {
        let now = Utc::now();
        let snapshot = RunnerSnapshot {
            status: "online".to_owned(),
            busy: false,
            worker_count: 0,
            oldest_worker_age_min: None,
        };
        let runs = vec![queued_run(
            101,
            "CI",
            "main",
            &(now - ChronoDuration::hours(3)).to_rfc3339(),
        )];
        let report = assess_runner(&snapshot, &runs, WatchdogThresholds::default(), now);
        assert_eq!(report.health, RunnerHealth::Stuck);
        assert!(matches!(
            report.symptoms[0],
            Symptom::StaleQueuedRuns { count: 1 }
        ));
        assert_eq!(report.stale_queued_runs.len(), 1);
    }

    fn run_with_status(
        id: u64,
        workflow: &str,
        branch: &str,
        status: &str,
        created_at: &str,
    ) -> QueuedRun {
        QueuedRun {
            database_id: id,
            name: workflow.to_owned(),
            head_branch: branch.to_owned(),
            created_at: created_at.to_owned(),
            run_started_at: None,
            workflow_name: workflow.to_owned(),
            url: Some(format!("https://github.com/owner/repo/actions/runs/{id}")),
            path: ".github/workflows/ci.yml".to_owned(),
            status: status.to_owned(),
            conclusion: None,
        }
    }

    /// Like [`run_with_status`] but also pins `run_started_at`. Used by the
    /// P1 hung-detection tests, where an `in_progress` run's age must be
    /// measured from execution start, not creation time.
    fn in_progress_run(
        id: u64,
        branch: &str,
        created_at: &str,
        run_started_at: Option<&str>,
    ) -> QueuedRun {
        QueuedRun {
            run_started_at: run_started_at.map(ToOwned::to_owned),
            ..run_with_status(id, "Coverage", branch, "in_progress", created_at)
        }
    }

    #[test]
    fn reaper_keeps_run_just_under_threshold() {
        let now = Utc::now();
        // in_progress threshold is 300 min by default; 299 min must be kept.
        let in_progress = vec![run_with_status(
            1,
            "Coverage",
            "main",
            "in_progress",
            &(now - ChronoDuration::minutes(299)).to_rfc3339(),
        )];
        let stale = compute_stale_runs(&in_progress, &[], ReaperThresholds::default(), now);
        assert!(stale.is_empty(), "a 299-min run is healthy, must be kept");
    }

    #[test]
    fn reaper_reaps_hung_in_progress_run_over_threshold() {
        let now = Utc::now();
        let in_progress = vec![run_with_status(
            42,
            "Coverage",
            "main",
            "in_progress",
            &(now - ChronoDuration::minutes(301)).to_rfc3339(),
        )];
        let stale = compute_stale_runs(&in_progress, &[], ReaperThresholds::default(), now);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].run_id, 42);
        assert_eq!(stale[0].kind, StaleRunKind::HungInProgress);
        assert!(stale[0].age_secs > 300 * 60);
    }

    #[test]
    fn reaper_reaps_orphaned_queued_run_over_threshold() {
        let now = Utc::now();
        // queued threshold is 480 min by default; 5 days is way past it.
        let queued = vec![run_with_status(
            7,
            "CI",
            "feature/orphan",
            "queued",
            &(now - ChronoDuration::days(5)).to_rfc3339(),
        )];
        let stale = compute_stale_runs(&[], &queued, ReaperThresholds::default(), now);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].run_id, 7);
        assert_eq!(stale[0].kind, StaleRunKind::OrphanedQueued);
    }

    #[test]
    fn reaper_keeps_queued_run_just_under_threshold() {
        let now = Utc::now();
        let queued = vec![run_with_status(
            8,
            "CI",
            "feature/recent",
            "queued",
            &(now - ChronoDuration::minutes(479)).to_rfc3339(),
        )];
        let stale = compute_stale_runs(&[], &queued, ReaperThresholds::default(), now);
        assert!(stale.is_empty());
    }

    #[test]
    fn reaper_skips_unparseable_timestamps() {
        let now = Utc::now();
        let in_progress = vec![run_with_status(
            9,
            "CI",
            "main",
            "in_progress",
            "not-a-timestamp",
        )];
        let stale = compute_stale_runs(&in_progress, &[], ReaperThresholds::default(), now);
        assert!(stale.is_empty());
    }

    #[test]
    fn reaper_classifies_both_kinds_in_one_pass() {
        let now = Utc::now();
        let in_progress = vec![run_with_status(
            100,
            "Coverage",
            "main",
            "in_progress",
            &(now - ChronoDuration::hours(7)).to_rfc3339(),
        )];
        let queued = vec![run_with_status(
            200,
            "CI",
            "feature/x",
            "queued",
            &(now - ChronoDuration::days(3)).to_rfc3339(),
        )];
        let stale = compute_stale_runs(&in_progress, &queued, ReaperThresholds::default(), now);
        assert_eq!(stale.len(), 2);
        assert_eq!(stale[0].kind, StaleRunKind::HungInProgress);
        assert_eq!(stale[1].kind, StaleRunKind::OrphanedQueued);
    }

    #[test]
    fn reaper_respects_custom_thresholds() {
        let now = Utc::now();
        let in_progress = vec![run_with_status(
            1,
            "CI",
            "main",
            "in_progress",
            &(now - ChronoDuration::minutes(45)).to_rfc3339(),
        )];
        // A tight 30-min in_progress threshold flags the 45-min run.
        let thresholds = ReaperThresholds {
            in_progress_max_min: 30,
            queued_max_min: 60,
        };
        let stale = compute_stale_runs(&in_progress, &[], thresholds, now);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].kind, StaleRunKind::HungInProgress);
    }

    #[test]
    fn reaper_keeps_recently_started_in_progress_run_with_old_created_at() {
        // P1: a run that sat queued for ~9h then started executing 10 min ago
        // is healthy. Aging it from `created_at` would (wrongly) cancel it;
        // aging it from `run_started_at` keeps it.
        let now = Utc::now();
        let in_progress = vec![in_progress_run(
            1,
            "main",
            &(now - ChronoDuration::hours(9)).to_rfc3339(),
            Some(&(now - ChronoDuration::minutes(10)).to_rfc3339()),
        )];
        let stale = compute_stale_runs(&in_progress, &[], ReaperThresholds::default(), now);
        assert!(
            stale.is_empty(),
            "an in_progress run started 10 min ago is healthy regardless of queue wait"
        );
    }

    #[test]
    fn reaper_reaps_in_progress_run_with_old_run_started_at() {
        // P1: a genuinely hung run — started executing 6h ago, past the
        // 300-min default — is still reaped when aged from `run_started_at`.
        let now = Utc::now();
        let in_progress = vec![in_progress_run(
            2,
            "main",
            &(now - ChronoDuration::hours(7)).to_rfc3339(),
            Some(&(now - ChronoDuration::hours(6)).to_rfc3339()),
        )];
        let stale = compute_stale_runs(&in_progress, &[], ReaperThresholds::default(), now);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].run_id, 2);
        assert_eq!(stale[0].kind, StaleRunKind::HungInProgress);
    }

    #[test]
    fn reaper_falls_back_to_created_at_when_run_started_at_missing() {
        // P1: GitHub can omit `run_started_at`; the reaper must not crash and
        // should fall back to `created_at` for in_progress runs.
        let now = Utc::now();
        let in_progress = vec![in_progress_run(
            3,
            "main",
            &(now - ChronoDuration::minutes(301)).to_rfc3339(),
            None,
        )];
        let stale = compute_stale_runs(&in_progress, &[], ReaperThresholds::default(), now);
        assert_eq!(
            stale.len(),
            1,
            "missing run_started_at falls back to created_at"
        );
        assert_eq!(stale[0].kind, StaleRunKind::HungInProgress);
    }

    #[test]
    fn reaper_queued_run_age_still_uses_created_at() {
        // P1: queued runs never started, so a (spurious) `run_started_at` must
        // be ignored — age is measured from `created_at`. Here `created_at` is
        // 5 days old (well past the 480-min queued threshold) so the run is
        // reaped even though a stray recent `run_started_at` is present.
        let now = Utc::now();
        let queued = vec![QueuedRun {
            run_started_at: Some((now - ChronoDuration::minutes(1)).to_rfc3339()),
            ..run_with_status(
                4,
                "CI",
                "feature/orphan",
                "queued",
                &(now - ChronoDuration::days(5)).to_rfc3339(),
            )
        }];
        let stale = compute_stale_runs(&[], &queued, ReaperThresholds::default(), now);
        assert_eq!(stale.len(), 1, "queued run age ignores run_started_at");
        assert_eq!(stale[0].run_id, 4);
        assert_eq!(stale[0].kind, StaleRunKind::OrphanedQueued);
    }

    #[test]
    fn report_to_json_includes_top_level_fields() {
        let now = Utc::now();
        let snapshot = RunnerSnapshot {
            status: "online".to_owned(),
            busy: true,
            worker_count: 0,
            oldest_worker_age_min: None,
        };
        let report = assess_runner(&snapshot, &[], WatchdogThresholds::default(), now);
        let data = report_to_json(&report);
        assert_eq!(data["health"], Value::from("stuck"));
        assert_eq!(data["status"], Value::from("online"));
        assert_eq!(data["busy"], Value::Bool(true));
        assert_eq!(data["worker_count"], Value::from(0));
    }
}
