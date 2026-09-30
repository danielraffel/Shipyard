#![allow(missing_docs)]

pub mod job_name;
pub mod proxy;
mod proxy_store;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Durable Shipyard metrics store backed by `SQLite`.
#[derive(Debug)]
pub struct MetricsStore {
    path: PathBuf,
}

/// Input for a low-friction `shipyard metrics record` call.
#[derive(Clone, Debug, Default)]
pub struct MetricRecordInput {
    pub project: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub sha: Option<String>,
    pub pr: Option<i64>,
    pub workflow: Option<String>,
    pub profile: Option<String>,
    pub routing_decision: Option<String>,
    pub job: String,
    pub target: Option<String>,
    pub platform: Option<String>,
    pub backend: Option<String>,
    pub provider: Option<String>,
    pub runner: Option<String>,
    pub host: Option<String>,
    pub step: Option<String>,
    pub duration_ms: i64,
    pub status: String,
    pub exit_code: Option<i64>,
    pub failure_class: Option<String>,
    pub external_id: Option<String>,
    /// Time the provider placed the job in its queue, when authoritative.
    pub queued_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Authoritative cache reuse observation for this step, when supplied.
    pub cache_hit: Option<bool>,
    /// Whether a runner was ever assigned to the job; `None` when unknown.
    /// Starvation (cancelled before any runner) is measured from this.
    pub runner_assigned: Option<bool>,
    /// Runner labels the job requested, when known.
    pub labels: Option<Vec<String>>,
}

/// One grouped timing summary row.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MetricsSummaryRow {
    pub project: String,
    pub target: String,
    pub backend: String,
    pub host: String,
    pub provider: String,
    pub count: usize,
    pub failures: usize,
    pub failure_rate: f64,
    pub min_ms: Option<i64>,
    pub p50_ms: Option<i64>,
    pub p90_ms: Option<i64>,
    pub max_ms: Option<i64>,
    pub avg_ms: Option<i64>,
}

/// One recent job row for `metrics list`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MetricsJobRow {
    pub project: String,
    pub repo: Option<String>,
    pub workflow: Option<String>,
    pub job: String,
    pub target: Option<String>,
    pub backend: Option<String>,
    pub provider: Option<String>,
    pub host: Option<String>,
    pub status: String,
    pub total_ms: Option<i64>,
    pub completed_at: Option<String>,
    pub external_id: Option<String>,
}

/// Agent-facing finding emitted by watch/advise.
#[derive(Clone, Debug, Serialize)]
pub struct MetricsFinding {
    pub severity: String,
    pub lane: String,
    pub signal: String,
    pub message: String,
    pub sample_count: usize,
    pub suggested_poll_interval_secs: u64,
    pub recommended_actions: Vec<String>,
    /// Basis of this finding's verdict: `proxy` (load-independent counts) or
    /// `wall_time` (load-dependent durations).
    pub basis: &'static str,
    /// Full before/after comparison, when the finding came from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<proxy::Comparison>,
    /// What the finding's sample counts count, when it states shares.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denominator: Option<Denominator>,
    /// Whether the lane holds a required status check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateClass>,
}

/// The denominator behind a lane's shares. Every share is over job rows, each
/// judged by that job's own conclusion; a workflow run's conclusion (which an
/// advisory job elsewhere in the run can turn red) is never used.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Denominator {
    /// Always `jobs`.
    pub unit: &'static str,
    /// Job rows in the earlier window.
    pub previous_jobs: usize,
    /// Job rows in the later window.
    pub current_jobs: usize,
    /// Earlier-window jobs that finished success or failure: the
    /// `failure_share` denominator.
    pub previous_decided: usize,
    /// Later-window jobs that finished success or failure.
    pub current_decided: usize,
    /// Distinct job (check) names in the lane.
    pub job_names: Vec<String>,
}

/// Whether a lane gates merges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateClass {
    /// Holds a required status check.
    Required,
    /// Holds only jobs that are not required checks.
    Advisory,
    /// No required-check set was supplied.
    Unclassified,
}

/// How `metrics summary` groups rows by machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryGroupBy {
    /// The recorded machine name: an ephemeral runner name when a job was
    /// imported from GitHub.
    #[default]
    Runner,
    /// The physical host behind the runner (see [`host_group`]).
    Host,
}

/// Compact, bounded stewardship scorecard. Fields that the metrics store does
/// not yet capture are reported explicitly instead of inferred.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StewardshipScorecard {
    pub project: String,
    pub since_days: i64,
    pub job_samples: usize,
    pub successful_jobs: usize,
    pub failed_jobs: usize,
    pub other_jobs: usize,
    pub failure_rate: Option<f64>,
    pub worker_minutes: f64,
    pub duration_samples: usize,
    pub worker_minutes_coverage: ScorecardCoverage,
    pub duration_p50_ms: Option<i64>,
    pub duration_p90_ms: Option<i64>,
    pub queue_samples: usize,
    pub queue_p50_ms: Option<i64>,
    pub queue_p90_ms: Option<i64>,
    pub distinct_pull_requests: usize,
    pub pull_requests_per_day: f64,
    pub pull_request_throughput: ScorecardCoverage,
    pub cache_samples: usize,
    pub cache_hits: usize,
    pub cache_hit_rate: Option<f64>,
    pub submit_to_receipt: ScorecardCoverage,
    pub model_token_use: ScorecardCoverage,
    /// Basis of `comparison.verdict`.
    pub basis: &'static str,
    /// Load-independent proxies over this window (project-wide).
    pub proxies: Vec<proxy::ProxyValue>,
    /// This window against the equal-length window before it. Duration
    /// fields above are wall-clock and load-dependent; they are context.
    pub comparison: proxy::Comparison,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScorecardCoverage {
    pub status: String,
    pub reason: String,
}

type ScorecardSample = (
    String,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<i64>,
);

impl MetricsStore {
    /// Open a metrics store under the selected Shipyard state directory.
    pub fn open(state_dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let dir = state_dir.join("metrics");
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&dir)?;
        fs::create_dir_all(&dir)?;
        let store = Self {
            path: dir.join("metrics.db"),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Return the `SQLite` database path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn connect(&self) -> Result<Connection, rusqlite::Error> {
        Connection::open(&self.path)
    }

    fn migrate(&self) -> Result<(), Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS machines (
              id INTEGER PRIMARY KEY,
              name TEXT NOT NULL,
              kind TEXT NOT NULL,
              os TEXT,
              arch TEXT,
              cpu_count INTEGER,
              ram_mb INTEGER,
              labels_json TEXT,
              UNIQUE(name, kind, os, arch)
            );
            CREATE TABLE IF NOT EXISTS runs (
              id INTEGER PRIMARY KEY,
              ts TEXT NOT NULL,
              project TEXT NOT NULL,
              repo TEXT,
              branch TEXT,
              sha TEXT,
              pr INTEGER,
              workflow TEXT,
              profile TEXT,
              routing_decision TEXT,
              status TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS jobs (
              id INTEGER PRIMARY KEY,
              run_id INTEGER NOT NULL REFERENCES runs(id),
              machine_id INTEGER REFERENCES machines(id),
              job TEXT NOT NULL,
              target TEXT,
              platform TEXT,
              backend TEXT,
              provider TEXT,
              queued_at TEXT,
              started_at TEXT,
              completed_at TEXT,
              queue_ms INTEGER,
              boot_ms INTEGER,
              setup_ms INTEGER,
              run_ms INTEGER,
              total_ms INTEGER,
              status TEXT NOT NULL,
              exit_code INTEGER,
              failure_class TEXT,
              external_id TEXT,
              UNIQUE(provider, external_id)
            );
            CREATE TABLE IF NOT EXISTS steps (
              id INTEGER PRIMARY KEY,
              job_id INTEGER NOT NULL REFERENCES jobs(id),
              step TEXT NOT NULL,
              started_at TEXT,
              completed_at TEXT,
              duration_ms INTEGER NOT NULL,
              status TEXT NOT NULL,
              cache_key TEXT,
              cache_hit INTEGER,
              artifact_path TEXT
            );
            ",
        )?;
        add_column_if_missing(&conn, "jobs", "runner_assigned", "INTEGER")?;
        add_column_if_missing(&conn, "jobs", "labels_json", "TEXT")?;
        Ok(())
    }

    /// Record one low-friction timing row.
    pub fn record(&self, input: &MetricRecordInput) -> Result<i64, Box<dyn std::error::Error>> {
        self.record_with_refresh(input, false)
    }

    /// Record a terminal external observation, refreshing a prior partial row
    /// with the same provider identity when necessary.
    pub(crate) fn record_terminal_observation(
        &self,
        input: &MetricRecordInput,
    ) -> Result<i64, Box<dyn std::error::Error>> {
        if input.completed_at.is_none() {
            return Err("terminal metrics observation requires completed_at".into());
        }
        self.record_with_refresh(input, true)
    }

    #[allow(clippy::too_many_lines)]
    fn record_with_refresh(
        &self,
        input: &MetricRecordInput,
        refresh_existing: bool,
    ) -> Result<i64, Box<dyn std::error::Error>> {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        let conn = self.connect()?;
        let completed_at = input.completed_at.unwrap_or_else(Utc::now);
        let started_at = input
            .started_at
            .unwrap_or_else(|| completed_at - Duration::milliseconds(input.duration_ms.max(0)));
        let measured_total_ms = measured_total_ms(input);
        let queued_at = input.queued_at.map(|value| value.to_rfc3339());
        let queue_ms = input
            .queued_at
            .zip(input.started_at)
            .map(|(queued, started)| (started - queued).num_milliseconds())
            .filter(|value| *value >= 0);
        if let Some(provider) = input.provider.as_deref()
            && let Some(existing) = existing_job_id(&conn, provider, input.external_id.as_deref())?
        {
            if refresh_existing {
                let machine_id = upsert_machine(
                    &conn,
                    input
                        .host
                        .as_deref()
                        .or(input.runner.as_deref())
                        .unwrap_or("unknown"),
                    input.backend.as_deref().unwrap_or("unknown"),
                    input.platform.as_deref(),
                    None,
                    input.runner.as_deref(),
                )?;
                refresh_existing_job(
                    &conn,
                    existing,
                    machine_id,
                    input,
                    started_at,
                    completed_at,
                    measured_total_ms,
                    queued_at.as_deref(),
                    queue_ms,
                )?;
            }
            return Ok(existing);
        }
        let machine_id = upsert_machine(
            &conn,
            input
                .host
                .as_deref()
                .or(input.runner.as_deref())
                .unwrap_or("unknown"),
            input.backend.as_deref().unwrap_or("unknown"),
            input.platform.as_deref(),
            None,
            input.runner.as_deref(),
        )?;
        let run_id = insert_run(
            &conn,
            &RunInsert {
                ts: completed_at.to_rfc3339(),
                project: input.project.clone(),
                repo: input.repo.clone(),
                branch: input.branch.clone(),
                sha: input.sha.clone(),
                pr: input.pr,
                workflow: input.workflow.clone(),
                profile: input.profile.clone(),
                routing_decision: input.routing_decision.clone(),
                status: input.status.clone(),
            },
        )?;
        let job_id = insert_job(
            &conn,
            &JobInsert {
                run_id,
                machine_id: Some(machine_id),
                job: input.job.clone(),
                target: input.target.clone(),
                platform: input.platform.clone(),
                backend: input.backend.clone(),
                provider: input.provider.clone(),
                queued_at,
                started_at: Some(started_at.to_rfc3339()),
                completed_at: Some(completed_at.to_rfc3339()),
                queue_ms,
                boot_ms: None,
                setup_ms: None,
                run_ms: measured_total_ms,
                total_ms: measured_total_ms,
                status: input.status.clone(),
                exit_code: input.exit_code,
                failure_class: input.failure_class.clone(),
                external_id: nonempty(input.external_id.clone()),
                runner_assigned: input.runner_assigned,
                labels_json: labels_json(input),
            },
        )?;
        let started_at_text = started_at.to_rfc3339();
        let completed_at_text = completed_at.to_rfc3339();
        insert_step(
            &conn,
            job_id,
            input.step.as_deref().unwrap_or("total"),
            Some(&started_at_text),
            Some(&completed_at_text),
            input.duration_ms,
            &input.status,
            input.cache_hit,
        )?;
        Ok(job_id)
    }

    /// Import tartci runtime export records from JSONL or JSON array text.
    pub fn import_tartci(&self, text: &str) -> Result<usize, Box<dyn std::error::Error>> {
        let values = parse_json_records(text)?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        let mut imported = 0;
        for value in values {
            if self.import_tartci_value(&value)? {
                imported += 1;
            }
        }
        Ok(imported)
    }

    fn import_tartci_value(&self, value: &Value) -> Result<bool, Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        let provider = value_str(value, "provider").unwrap_or("tartci").to_owned();
        let external_id = nonempty(value_str(value, "external_id").map(str::to_owned));
        if let Some(existing) = existing_job_id(&conn, &provider, external_id.as_deref())? {
            import_phases(&conn, existing, value)?;
            return Ok(false);
        }

        let host = value_str(value, "host")
            .or_else(|| value_str(value, "runner_name"))
            .unwrap_or("unknown");
        let machine_id = upsert_machine(
            &conn,
            host,
            value_str(value, "backend").unwrap_or("vm"),
            value_str(value, "platform"),
            value_str(value, "arch"),
            value.get("labels").map(Value::to_string).as_deref(),
        )?;
        let status = value_str(value, "status").unwrap_or("unknown").to_owned();
        let ts = value_str(value, "completed_at")
            .or_else(|| value_str(value, "started_at"))
            .map_or_else(|| Utc::now().to_rfc3339(), str::to_owned);
        let run_id = insert_run(
            &conn,
            &RunInsert {
                ts,
                project: value_str(value, "project").unwrap_or("unknown").to_owned(),
                repo: value_str(value, "repo").map(str::to_owned),
                branch: value_str(value, "branch").map(str::to_owned),
                sha: value_str(value, "sha").map(str::to_owned),
                pr: value_i64(value, "pr"),
                workflow: value_str(value, "workflow").map(str::to_owned),
                profile: value_str(value, "profile").map(str::to_owned),
                routing_decision: value_str(value, "routing_decision").map(str::to_owned),
                status: status.clone(),
            },
        )?;
        let job_id = insert_job(
            &conn,
            &JobInsert {
                run_id,
                machine_id: Some(machine_id),
                job: value_str(value, "job").unwrap_or("unknown").to_owned(),
                target: value_str(value, "target").map(str::to_owned),
                platform: value_str(value, "platform").map(str::to_owned),
                backend: value_str(value, "backend").map(str::to_owned),
                provider: Some(provider),
                queued_at: value_str(value, "queued_at").map(str::to_owned),
                started_at: value_str(value, "started_at").map(str::to_owned),
                completed_at: value_str(value, "completed_at").map(str::to_owned),
                queue_ms: value_i64(value, "queue_ms"),
                boot_ms: value_i64(value, "boot_ms"),
                setup_ms: value_i64(value, "setup_ms"),
                run_ms: value_i64(value, "run_ms"),
                total_ms: value_i64(value, "total_ms"),
                status,
                exit_code: value_i64(value, "exit_code"),
                failure_class: value_str(value, "failure_class").map(str::to_owned),
                external_id,
                runner_assigned: value.get("runner_assigned").and_then(Value::as_bool),
                labels_json: value.get("labels").map(Value::to_string),
            },
        )?;
        import_phases(&conn, job_id, value)?;
        Ok(true)
    }

    /// Return recent job rows.
    pub fn list(
        &self,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MetricsJobRow>, Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        let mut rows = load_jobs(&conn, project)?;
        rows.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));
        rows.truncate(limit);
        Ok(rows)
    }

    /// Group by project/target/backend/host/provider and compute basic stats.
    pub fn summary(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<MetricsSummaryRow>, Box<dyn std::error::Error>> {
        self.summary_grouped(project, SummaryGroupBy::Runner)
    }

    /// [`Self::summary`], grouping machines by `group_by`.
    pub fn summary_grouped(
        &self,
        project: Option<&str>,
        group_by: SummaryGroupBy,
    ) -> Result<Vec<MetricsSummaryRow>, Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        let mut rows = load_summary_inputs(&conn, project)?;
        if group_by == SummaryGroupBy::Host {
            for row in &mut rows {
                row.host = host_group(&row.host, &row.labels, row.repo.as_deref());
            }
        }
        Ok(group_summary(rows))
    }

    /// Slowest successful jobs.
    pub fn slowest(
        &self,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MetricsJobRow>, Box<dyn std::error::Error>> {
        let mut rows = self.list(project, usize::MAX)?;
        rows.retain(|row| row.status == "pass" || row.status == "success");
        rows.sort_by_key(|row| std::cmp::Reverse(row.total_ms.unwrap_or_default()));
        rows.truncate(limit);
        Ok(rows)
    }

    /// Agent-oriented watch findings for material regressions. The verdict
    /// uses `basis`: load-independent proxies by default, or wall-clock p90.
    pub fn watch(
        &self,
        project: &str,
        since_days: i64,
        basis: proxy::Basis,
    ) -> Result<Vec<MetricsFinding>, Box<dyn std::error::Error>> {
        self.watch_with_required(project, since_days, basis, &[])
    }

    /// [`Self::watch`], classifying each lane as a required gate or advisory
    /// against `required` (status-check names). An empty `required` leaves
    /// every lane `unclassified`.
    pub fn watch_with_required(
        &self,
        project: &str,
        since_days: i64,
        basis: proxy::Basis,
        required: &[String],
    ) -> Result<Vec<MetricsFinding>, Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        let rows = load_summary_inputs(&conn, Some(project))?;
        let wall = watch_findings(rows, since_days);
        if basis == proxy::Basis::WallTime {
            return Ok(wall);
        }
        let samples = proxy_store::load_samples(&conn, Some(project))?;
        Ok(proxy_store::watch_findings(
            &samples,
            since_days,
            Utc::now(),
            required,
        ))
    }

    /// Every lane's earlier-half vs later-half comparison over `since_days`.
    pub fn trend(
        &self,
        project: Option<&str>,
        since_days: i64,
        basis: proxy::Basis,
    ) -> Result<Vec<MetricsFinding>, Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        let samples = proxy_store::load_samples(&conn, project)?;
        Ok(proxy_store::trend_findings(
            &samples,
            since_days,
            Utc::now(),
            basis,
        ))
    }

    /// Recommend the fastest healthy lane for each target.
    ///
    /// Lanes are keyed by the job's resolved name (an unevaluated `${{ }}`
    /// name joins the job it evaluates to) and by physical host, not by the
    /// ephemeral runner, so a gate served by throwaway runners still
    /// accumulates samples. Only success and failure count; skipped,
    /// cancelled and unfinished jobs say nothing about a lane's health.
    pub fn advise(&self, project: &str) -> Result<Vec<MetricsFinding>, Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        let rows = load_summary_inputs(&conn, Some(project))?;
        Ok(advise_findings(rows))
    }

    /// Compare before/after windows split at `split_days_ago`. The verdict
    /// uses `basis`: load-independent proxies by default, or wall-clock p50.
    pub fn compare(
        &self,
        project: &str,
        split_days_ago: i64,
        basis: proxy::Basis,
    ) -> Result<Vec<MetricsFinding>, Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        if basis == proxy::Basis::WallTime {
            let rows = load_summary_inputs(&conn, Some(project))?;
            return Ok(compare_findings(rows, split_days_ago));
        }
        let samples = proxy_store::load_samples(&conn, Some(project))?;
        Ok(proxy_store::compare_findings(
            &samples,
            split_days_ago,
            Utc::now(),
        ))
    }

    /// Return one compact historical scorecard without producing per-lane
    /// findings or pretending absent telemetry was measured. The comparison
    /// verdict uses load-independent proxies.
    pub fn stewardship_scorecard(
        &self,
        project: &str,
        since_days: i64,
    ) -> Result<StewardshipScorecard, Box<dyn std::error::Error>> {
        self.stewardship_scorecard_with_basis(project, since_days, proxy::Basis::Proxy)
    }

    /// [`Self::stewardship_scorecard`] with an explicit verdict basis.
    #[allow(clippy::too_many_lines)]
    pub fn stewardship_scorecard_with_basis(
        &self,
        project: &str,
        since_days: i64,
        basis: proxy::Basis,
    ) -> Result<StewardshipScorecard, Box<dyn std::error::Error>> {
        validate_scorecard_scope(project, since_days)?;
        let conn = self.connect()?;
        let window = Duration::try_days(since_days).ok_or("scorecard day window is too large")?;
        let cutoff = Utc::now()
            .checked_sub_signed(window)
            .ok_or("scorecard day window is outside the timestamp range")?
            .to_rfc3339();
        let samples = load_scorecard_samples(&conn, project, &cutoff)?;
        let successful_jobs = samples
            .iter()
            .filter(|(status, _, _, _, _)| is_success_status(status))
            .count();
        let failed_jobs = samples
            .iter()
            .filter(|(status, _, _, _, _)| is_failure_status(status))
            .count();
        let classified_jobs = successful_jobs + failed_jobs;
        let other_jobs = samples.len().saturating_sub(classified_jobs);
        let mut durations = samples
            .iter()
            .filter_map(|(_, duration, _, _, _)| *duration)
            .filter(|value| *value >= 0)
            .collect::<Vec<_>>();
        durations.sort_unstable();
        let worker_minutes_coverage = coverage_for_samples(
            durations.len(),
            samples.len(),
            "completed job samples include measured duration",
        );
        let mut queues = samples
            .iter()
            .filter_map(|(_, _, queue, _, _)| *queue)
            .filter(|value| *value >= 0)
            .collect::<Vec<_>>();
        queues.sort_unstable();
        let distinct_pull_requests = samples
            .iter()
            .filter_map(|(_, _, _, repo, pr)| Some((repo.as_deref()?, (*pr)?)))
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let identified_pull_request_samples = samples
            .iter()
            .filter(|(_, _, _, repo, pr)| repo.is_some() && pr.is_some())
            .count();
        let pull_request_throughput = if samples.is_empty() || identified_pull_request_samples == 0
        {
            ScorecardCoverage {
                status: "unavailable".to_owned(),
                reason: "no completed job samples include pull-request identity".to_owned(),
            }
        } else if identified_pull_request_samples == samples.len() {
            ScorecardCoverage {
                status: "available".to_owned(),
                reason: "every completed job sample includes pull-request identity".to_owned(),
            }
        } else {
            ScorecardCoverage {
                status: "partial".to_owned(),
                reason: format!(
                    "{identified_pull_request_samples} of {} completed job samples include pull-request identity",
                    samples.len()
                ),
            }
        };
        let cache = load_scorecard_cache_samples(&conn, project, &cutoff)?;
        let cache_hits = cache.iter().filter(|hit| **hit).count();
        let now = Utc::now();
        let (proxies, comparison) = proxy_store::scorecard_window(
            &proxy_store::load_samples(&conn, Some(project))?,
            now,
            window,
            basis,
        )
        .ok_or("scorecard day window is outside the timestamp range")?;
        let job_samples = samples.len();
        Ok(StewardshipScorecard {
            project: project.to_owned(),
            since_days,
            job_samples,
            successful_jobs,
            failed_jobs,
            other_jobs,
            failure_rate: ratio(failed_jobs, classified_jobs),
            worker_minutes: worker_minutes(&durations),
            duration_samples: durations.len(),
            worker_minutes_coverage,
            duration_p50_ms: percentile(&durations, 50),
            duration_p90_ms: percentile(&durations, 90),
            queue_samples: queues.len(),
            queue_p50_ms: percentile(&queues, 50),
            queue_p90_ms: percentile(&queues, 90),
            distinct_pull_requests,
            pull_requests_per_day: pull_requests_per_day(distinct_pull_requests, since_days),
            pull_request_throughput,
            cache_samples: cache.len(),
            cache_hits,
            cache_hit_rate: ratio(cache_hits, cache.len()),
            submit_to_receipt: ScorecardCoverage {
                status: "unavailable".to_owned(),
                reason: "job metrics do not store durable submission and receipt timestamps"
                    .to_owned(),
            },
            model_token_use: ScorecardCoverage {
                status: "unavailable".to_owned(),
                reason: "job metrics do not store model call or token counters".to_owned(),
            },
            basis: basis.as_str(),
            proxies,
            comparison,
        })
    }
}

fn validate_scorecard_scope(
    project: &str,
    since_days: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    if project.trim().is_empty() || since_days <= 0 {
        return Err("scorecard requires a project and positive day window".into());
    }
    Ok(())
}

fn is_success_status(status: &str) -> bool {
    matches!(status, "pass" | "success")
}

fn is_failure_status(status: &str) -> bool {
    matches!(
        status,
        "failure" | "failed" | "timed_out" | "action_required" | "startup_failure"
    )
}

fn load_scorecard_samples(
    conn: &Connection,
    project: &str,
    cutoff: &str,
) -> Result<Vec<ScorecardSample>, rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT job.status, job.total_ms, job.queue_ms, run.repo, run.pr
           FROM jobs job JOIN runs run ON run.id = job.run_id
          WHERE run.project = ?1 AND job.completed_at IS NOT NULL
            AND julianday(job.completed_at) >= julianday(?2)
          ORDER BY job.id",
    )?;
    statement
        .query_map(params![project, cutoff], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect()
}

fn load_scorecard_cache_samples(
    conn: &Connection,
    project: &str,
    cutoff: &str,
) -> Result<Vec<bool>, rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT step.cache_hit
           FROM steps step
           JOIN jobs job ON job.id = step.job_id
           JOIN runs run ON run.id = job.run_id
          WHERE run.project = ?1 AND job.completed_at IS NOT NULL
            AND julianday(job.completed_at) >= julianday(?2)
            AND step.cache_hit IS NOT NULL",
    )?;
    statement
        .query_map(params![project, cutoff], |row| row.get(0))?
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn worker_minutes(durations: &[i64]) -> f64 {
    durations
        .iter()
        .map(|duration| *duration as f64)
        .sum::<f64>()
        / 60_000.0
}

#[allow(clippy::cast_precision_loss)]
fn pull_requests_per_day(distinct_pull_requests: usize, since_days: i64) -> f64 {
    distinct_pull_requests as f64 / since_days as f64
}

#[allow(clippy::cast_precision_loss)]
fn ratio(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator != 0).then(|| numerator as f64 / denominator as f64)
}

fn coverage_for_samples(measured: usize, total: usize, description: &str) -> ScorecardCoverage {
    let status = if measured == 0 {
        "unavailable"
    } else if measured == total {
        "available"
    } else {
        "partial"
    };
    ScorecardCoverage {
        status: status.to_owned(),
        reason: format!("{measured} of {total} {description}"),
    }
}

#[derive(Debug)]
struct RunInsert {
    ts: String,
    project: String,
    repo: Option<String>,
    branch: Option<String>,
    sha: Option<String>,
    pr: Option<i64>,
    workflow: Option<String>,
    profile: Option<String>,
    routing_decision: Option<String>,
    status: String,
}

#[derive(Debug)]
struct JobInsert {
    run_id: i64,
    machine_id: Option<i64>,
    job: String,
    target: Option<String>,
    platform: Option<String>,
    backend: Option<String>,
    provider: Option<String>,
    queued_at: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    queue_ms: Option<i64>,
    boot_ms: Option<i64>,
    setup_ms: Option<i64>,
    run_ms: Option<i64>,
    total_ms: Option<i64>,
    status: String,
    exit_code: Option<i64>,
    failure_class: Option<String>,
    external_id: Option<String>,
    runner_assigned: Option<bool>,
    labels_json: Option<String>,
}

#[derive(Clone, Debug)]
struct SummaryInput {
    project: String,
    target: String,
    backend: String,
    host: String,
    provider: String,
    status: String,
    total_ms: Option<i64>,
    completed_at: Option<DateTime<Utc>>,
    repo: Option<String>,
    labels: Vec<String>,
}

fn upsert_machine(
    conn: &Connection,
    name: &str,
    kind: &str,
    os: Option<&str>,
    arch: Option<&str>,
    labels_json: Option<&str>,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO machines (name, kind, os, arch, labels_json) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![name, kind, os, arch, labels_json],
    )?;
    conn.query_row(
        "SELECT id FROM machines WHERE name = ?1 AND kind = ?2 AND os IS ?3 AND arch IS ?4",
        params![name, kind, os, arch],
        |row| row.get(0),
    )
}

fn insert_run(conn: &Connection, run: &RunInsert) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO runs (ts, project, repo, branch, sha, pr, workflow, profile, routing_decision, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            run.ts,
            run.project,
            run.repo,
            run.branch,
            run.sha,
            run.pr,
            run.workflow,
            run.profile,
            run.routing_decision,
            run.status
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn insert_job(conn: &Connection, job: &JobInsert) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO jobs (
          run_id, machine_id, job, target, platform, backend, provider,
          queued_at, started_at, completed_at, queue_ms, boot_ms, setup_ms,
          run_ms, total_ms, status, exit_code, failure_class, external_id,
          runner_assigned, labels_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            job.run_id,
            job.machine_id,
            job.job,
            job.target,
            job.platform,
            job.backend,
            job.provider,
            job.queued_at,
            job.started_at,
            job.completed_at,
            job.queue_ms,
            job.boot_ms,
            job.setup_ms,
            job.run_ms,
            job.total_ms,
            job.status,
            job.exit_code,
            job.failure_class,
            job.external_id,
            job.runner_assigned,
            job.labels_json
        ],
    )?;
    if conn.changes() == 0
        && let Some(id) = existing_job_id(
            conn,
            job.provider.as_deref().unwrap_or(""),
            job.external_id.as_deref(),
        )?
    {
        return Ok(id);
    }
    Ok(conn.last_insert_rowid())
}

#[allow(clippy::too_many_arguments)]
fn insert_step(
    conn: &Connection,
    job_id: i64,
    step: &str,
    started_at: Option<&str>,
    completed_at: Option<&str>,
    duration_ms: i64,
    status: &str,
    cache_hit: Option<bool>,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO steps (job_id, step, started_at, completed_at, duration_ms, status, cache_hit)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            job_id,
            step,
            started_at,
            completed_at,
            duration_ms,
            status,
            cache_hit
        ],
    )?;
    Ok(())
}

fn existing_job_id(
    conn: &Connection,
    provider: &str,
    external_id: Option<&str>,
) -> Result<Option<i64>, rusqlite::Error> {
    let Some(external_id) = external_id.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    conn.query_row(
        "SELECT id FROM jobs WHERE provider = ?1 AND external_id = ?2",
        params![provider, external_id],
        |row| row.get(0),
    )
    .optional()
}

#[allow(clippy::too_many_arguments)]
fn refresh_existing_job(
    conn: &Connection,
    job_id: i64,
    machine_id: i64,
    input: &MetricRecordInput,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    measured_total_ms: Option<i64>,
    queued_at: Option<&str>,
    queue_ms: Option<i64>,
) -> Result<(), rusqlite::Error> {
    let started_at = started_at.to_rfc3339();
    let completed_at = completed_at.to_rfc3339();
    conn.execute(
        "UPDATE jobs
            SET queued_at = COALESCE(?2, queued_at), started_at = ?3, completed_at = ?4, run_ms = ?5, total_ms = ?5,
                queue_ms = COALESCE(?6, queue_ms), status = ?7, exit_code = COALESCE(?8, exit_code),
                failure_class = COALESCE(?9, failure_class), machine_id = ?10,
                target = COALESCE(?11, target), platform = COALESCE(?12, platform),
                backend = COALESCE(?13, backend), provider = COALESCE(?14, provider),
                runner_assigned = COALESCE(?15, runner_assigned),
                labels_json = COALESCE(?16, labels_json)
          WHERE id = ?1",
        params![
            job_id,
            queued_at,
            started_at,
            completed_at,
            measured_total_ms,
            queue_ms,
            input.status,
            input.exit_code,
            input.failure_class,
            machine_id,
            input.target,
            input.platform,
            input.backend,
            input.provider,
            input.runner_assigned,
            labels_json(input),
        ],
    )?;
    conn.execute(
        "UPDATE runs
            SET ts = ?2, project = ?3, repo = COALESCE(?4, repo),
                branch = COALESCE(?5, branch), sha = COALESCE(?6, sha),
                pr = COALESCE(?7, pr), workflow = COALESCE(?8, workflow),
                profile = COALESCE(?9, profile),
                routing_decision = COALESCE(?10, routing_decision), status = ?11
          WHERE id = (SELECT run_id FROM jobs WHERE id = ?1)",
        params![
            job_id,
            completed_at,
            input.project,
            input.repo,
            input.branch,
            input.sha,
            input.pr,
            input.workflow,
            input.profile,
            input.routing_decision,
            input.status,
        ],
    )?;
    let updated_steps = conn.execute(
        "UPDATE steps
            SET step = ?2, started_at = ?3, completed_at = ?4,
                duration_ms = ?5, status = ?6
          WHERE job_id = ?1",
        params![
            job_id,
            input.step.as_deref().unwrap_or("total"),
            started_at,
            completed_at,
            input.duration_ms,
            input.status,
        ],
    )?;
    if updated_steps == 0 {
        insert_step(
            conn,
            job_id,
            input.step.as_deref().unwrap_or("total"),
            Some(&started_at),
            Some(&completed_at),
            input.duration_ms,
            &input.status,
            input.cache_hit,
        )?;
    }
    Ok(())
}

fn labels_json(input: &MetricRecordInput) -> Option<String> {
    input
        .labels
        .as_ref()
        .and_then(|labels| serde_json::to_string(labels).ok())
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    kind: &str,
) -> Result<(), rusqlite::Error> {
    let present: bool = conn.query_row(
        &format!("SELECT COUNT(*) > 0 FROM pragma_table_info('{table}') WHERE name = ?1"),
        params![column],
        |row| row.get(0),
    )?;
    if !present {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
    }
    Ok(())
}

fn measured_total_ms(input: &MetricRecordInput) -> Option<i64> {
    if input.step.as_deref() == Some("github_job")
        && (input.started_at.is_none() || input.completed_at.is_none())
    {
        None
    } else {
        Some(input.duration_ms)
    }
}

fn import_phases(
    conn: &Connection,
    job_id: i64,
    value: &Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(phases) = value.get("phases_ms").and_then(Value::as_object) else {
        return Ok(());
    };
    for (phase, duration) in phases {
        if let Some(ms) = duration.as_i64() {
            insert_step(
                conn,
                job_id,
                phase,
                None,
                None,
                ms,
                value_str(value, "status").unwrap_or("unknown"),
                value.get("cache_hit").and_then(Value::as_bool),
            )?;
        }
    }
    Ok(())
}

fn parse_json_records(text: &str) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        return Ok(serde_json::from_str(trimmed)?);
    }
    let mut values = Vec::new();
    for line in trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        values.push(serde_json::from_str(line)?);
    }
    Ok(values)
}

fn load_jobs(
    conn: &Connection,
    project: Option<&str>,
) -> Result<Vec<MetricsJobRow>, Box<dyn std::error::Error>> {
    let mut stmt = conn.prepare(
        "SELECT runs.project, runs.repo, runs.workflow, jobs.job, jobs.target, jobs.backend,
                jobs.provider, machines.name, jobs.status, jobs.total_ms, jobs.completed_at,
                jobs.external_id
         FROM jobs
         JOIN runs ON runs.id = jobs.run_id
         LEFT JOIN machines ON machines.id = jobs.machine_id
         WHERE (?1 IS NULL OR runs.project = ?1)",
    )?;
    let rows = stmt.query_map(params![project], |row| {
        Ok(MetricsJobRow {
            project: row.get(0)?,
            repo: row.get(1)?,
            workflow: row.get(2)?,
            job: row.get(3)?,
            target: row.get(4)?,
            backend: row.get(5)?,
            provider: row.get(6)?,
            host: row.get(7)?,
            status: row.get(8)?,
            total_ms: row.get(9)?,
            completed_at: row.get(10)?,
            external_id: row.get(11)?,
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

fn load_summary_inputs(
    conn: &Connection,
    project: Option<&str>,
) -> Result<Vec<SummaryInput>, Box<dyn std::error::Error>> {
    let mut stmt = conn.prepare(
        "SELECT runs.project,
                COALESCE(jobs.target, jobs.job, 'unknown'),
                COALESCE(jobs.backend, 'unknown'),
                COALESCE(machines.name, 'unknown'),
                COALESCE(jobs.provider, 'unknown'),
                jobs.status,
                jobs.total_ms,
                jobs.completed_at,
                runs.repo,
                jobs.labels_json
         FROM jobs
         JOIN runs ON runs.id = jobs.run_id
         LEFT JOIN machines ON machines.id = jobs.machine_id
         WHERE (?1 IS NULL OR runs.project = ?1)",
    )?;
    let rows = stmt.query_map(params![project], |row| {
        let completed_raw: Option<String> = row.get(7)?;
        let labels_raw: Option<String> = row.get(9)?;
        Ok(SummaryInput {
            project: row.get(0)?,
            target: row.get(1)?,
            backend: row.get(2)?,
            host: row.get(3)?,
            provider: row.get(4)?,
            status: row.get(5)?,
            total_ms: row.get(6)?,
            completed_at: completed_raw
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc)),
            repo: row.get(8)?,
            labels: labels_raw
                .as_deref()
                .and_then(|text| serde_json::from_str::<Vec<String>>(text).ok())
                .unwrap_or_default(),
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// Physical host behind a recorded machine name, for grouping. In order:
///
/// 1. a `<anything>-host-<id>` runner label names the host (`<id>`);
/// 2. a GitHub-hosted runner (`GitHub Actions 1000…`) is `github-hosted`;
/// 3. a just-in-time runner named `<tag>-<repo>-…` (`m5studio-pulp-gate-01-42746-25`)
///    is `<tag>`, the machine's `shipyard runner tag`;
/// 4. a registered runner named `<repo>-<tag>-NN` is `<tag>`;
/// 5. otherwise the name with trailing numeric and `slotN` segments removed,
///    which leaves a name that is already a host (`m3`) unchanged.
#[must_use]
pub fn host_group(machine: &str, labels: &[String], repo: Option<&str>) -> String {
    if let Some(id) = labels.iter().find_map(|label| {
        label
            .to_ascii_lowercase()
            .split_once("-host-")
            .map(|(_, id)| id.to_owned())
            .filter(|id| !id.is_empty())
    }) {
        return id;
    }
    let name = machine.trim();
    if name.is_empty() {
        return "unknown".to_owned();
    }
    if name.starts_with("GitHub Actions") {
        return "github-hosted".to_owned();
    }
    let lower = name.to_ascii_lowercase();
    if let Some(short) = repo
        .and_then(|repo| repo.rsplit('/').next())
        .map(str::to_ascii_lowercase)
        .filter(|short| !short.is_empty())
    {
        if let Some(index) = lower.find(&format!("-{short}-"))
            && index > 0
        {
            return lower[..index].to_owned();
        }
        if let Some(rest) = lower.strip_prefix(&format!("{short}-")) {
            let tag = strip_runner_index(rest);
            if !tag.is_empty() && tag != rest {
                return tag;
            }
        }
    }
    let stripped = strip_runner_index(&lower);
    if stripped.is_empty() {
        name.to_owned()
    } else {
        stripped
    }
}

/// Drop trailing all-digit and `slotN` segments from a runner name.
fn strip_runner_index(name: &str) -> String {
    let mut segments: Vec<&str> = name.split('-').collect();
    while let Some(last) = segments.last() {
        let slot = last
            .strip_prefix("slot")
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
        if slot || (!last.is_empty() && last.chars().all(|c| c.is_ascii_digit())) {
            segments.pop();
        } else {
            break;
        }
    }
    segments.join("-")
}

fn group_summary(rows: Vec<SummaryInput>) -> Vec<MetricsSummaryRow> {
    let mut groups: BTreeMap<(String, String, String, String, String), Vec<SummaryInput>> =
        BTreeMap::new();
    for row in rows {
        groups
            .entry((
                row.project.clone(),
                row.target.clone(),
                row.backend.clone(),
                row.host.clone(),
                row.provider.clone(),
            ))
            .or_default()
            .push(row);
    }
    groups
        .into_iter()
        .map(|((project, target, backend, host, provider), rows)| {
            let mut successful = rows
                .iter()
                .filter(|row| row.status == "pass" || row.status == "success")
                .filter_map(|row| row.total_ms)
                .collect::<Vec<_>>();
            successful.sort_unstable();
            let failures = rows
                .iter()
                .filter(|row| row.status != "pass" && row.status != "success")
                .count();
            let count = rows.len();
            MetricsSummaryRow {
                project,
                target,
                backend,
                host,
                provider,
                count,
                failures,
                failure_rate: failure_rate(failures, count),
                min_ms: successful.first().copied(),
                p50_ms: percentile(&successful, 50),
                p90_ms: percentile(&successful, 90),
                max_ms: successful.last().copied(),
                avg_ms: average(&successful),
            }
        })
        .collect()
}

fn watch_findings(rows: Vec<SummaryInput>, since_days: i64) -> Vec<MetricsFinding> {
    let now = Utc::now();
    let current_start = now - Duration::days(since_days.max(1));
    let previous_start = current_start - Duration::days(since_days.max(1));
    let mut groups: BTreeMap<String, (Vec<i64>, Vec<i64>)> = BTreeMap::new();
    for row in rows {
        if !(row.status == "pass" || row.status == "success") {
            continue;
        }
        let Some(completed_at) = row.completed_at else {
            continue;
        };
        let Some(total_ms) = row.total_ms else {
            continue;
        };
        let lane = format!("{}/{}/{}", row.target, row.backend, row.host);
        if completed_at >= current_start {
            groups.entry(lane).or_default().1.push(total_ms);
        } else if completed_at >= previous_start {
            groups.entry(lane).or_default().0.push(total_ms);
        }
    }
    let mut findings = Vec::new();
    for (lane, (mut previous, mut current)) in groups {
        previous.sort_unstable();
        current.sort_unstable();
        if previous.len() < 3 || current.len() < 3 {
            findings.push(MetricsFinding {
                severity: "info".to_owned(),
                lane,
                signal: "insufficient_samples".to_owned(),
                message: "Not enough historical samples for material drift detection.".to_owned(),
                sample_count: previous.len() + current.len(),
                suggested_poll_interval_secs: 600,
                recommended_actions: vec!["Keep collecting runner timing samples.".to_owned()],
                basis: proxy::Basis::WallTime.as_str(),
                comparison: None,
                denominator: None,
                gate: None,
            });
            continue;
        }
        let previous_p90 = percentile(&previous, 90).unwrap_or_default();
        let current_p90 = percentile(&current, 90).unwrap_or_default();
        if previous_p90 > 0 && current_p90 * 100 >= previous_p90 * 125 {
            findings.push(MetricsFinding {
                severity: "investigate".to_owned(),
                lane,
                signal: "p90_total_ms_regression".to_owned(),
                message: format!(
                    "p90 total runtime increased from {previous_p90}ms to {current_p90}ms."
                ),
                sample_count: current.len(),
                suggested_poll_interval_secs: 300,
                recommended_actions: vec![
                    "Check queue, boot, setup, and run-time splits.".to_owned(),
                    "Compare recent cache/golden/image labels against the previous window."
                        .to_owned(),
                ],
                basis: proxy::Basis::WallTime.as_str(),
                comparison: None,
                denominator: None,
                gate: None,
            });
        }
    }
    findings
}

/// Minimum decided (success or failure) jobs before a lane is advisable.
const ADVISE_MIN_DECIDED: usize = 3;
/// Failure ceiling for a lane to count as healthy.
const ADVISE_MAX_FAILURE_RATE: f64 = 0.10;

/// `(backend, host, provider)` of one advise lane.
type AdviseKey = (String, String, String);
/// One scored advise lane: key, failure rate, success p50, decided jobs.
type ScoredLane<'a> = (&'a AdviseKey, f64, Option<i64>, usize);

/// One advise lane: a resolved job name on one physical host.
#[derive(Debug, Default)]
struct AdviseLane {
    successes: usize,
    failures: usize,
    success_ms: Vec<i64>,
}

impl AdviseLane {
    const fn decided(&self) -> usize {
        self.successes + self.failures
    }
}

#[allow(clippy::too_many_lines)]
fn advise_findings(rows: Vec<SummaryInput>) -> Vec<MetricsFinding> {
    let mut by_target: BTreeMap<String, BTreeMap<AdviseKey, AdviseLane>> = BTreeMap::new();
    for row in rows {
        let success = proxy::is_success(&row.status);
        if !success && !proxy::is_failure(&row.status) {
            continue;
        }
        let target = job_name::canonical(&row.target);
        let host = host_group(&row.host, &row.labels, row.repo.as_deref());
        let lane = by_target
            .entry(target)
            .or_default()
            .entry((row.backend, host, row.provider))
            .or_default();
        if success {
            lane.successes += 1;
            lane.success_ms.extend(row.total_ms);
        } else {
            lane.failures += 1;
        }
    }
    let mut findings = Vec::new();
    for (target, lanes) in by_target {
        let total: usize = lanes.values().map(AdviseLane::decided).sum();
        let mut scored: Vec<ScoredLane<'_>> = lanes
            .iter()
            .map(|(key, lane)| {
                let mut timed = lane.success_ms.clone();
                timed.sort_unstable();
                (
                    key,
                    failure_rate(lane.failures, lane.decided()),
                    percentile(&timed, 50),
                    lane.decided(),
                )
            })
            .collect();
        scored.sort_by_key(|(_, _, p50, _)| p50.unwrap_or(i64::MAX));
        let viable = scored.iter().find(|(_, rate, p50, decided)| {
            *decided >= ADVISE_MIN_DECIDED && *rate <= ADVISE_MAX_FAILURE_RATE && p50.is_some()
        });
        if let Some(((backend, host, _), rate, p50, decided)) = viable {
            findings.push(MetricsFinding {
                severity: "info".to_owned(),
                lane: target.clone(),
                signal: "preferred_lane".to_owned(),
                message: format!(
                    "Prefer {backend} on {host} for {target}: fastest healthy p50 is {}ms over \
                     {decided} decided jobs (failure rate {rate:.2}).",
                    p50.unwrap_or_default(),
                ),
                sample_count: *decided,
                suggested_poll_interval_secs: 600,
                recommended_actions: vec![
                    "Keep the profile unchanged unless capacity or fidelity requirements disagree."
                        .to_owned(),
                ],
                basis: proxy::Basis::WallTime.as_str(),
                comparison: None,
                denominator: None,
                gate: None,
            });
            continue;
        }
        let sampled: Vec<String> = scored
            .iter()
            .filter(|(_, _, _, decided)| *decided >= ADVISE_MIN_DECIDED)
            .map(|((backend, host, _), rate, _, decided)| {
                format!("{backend}/{host} {:.0}% of {decided}", rate * 100.0)
            })
            .collect();
        if sampled.is_empty() {
            findings.push(MetricsFinding {
                severity: "watch".to_owned(),
                lane: target,
                signal: "insufficient_healthy_samples".to_owned(),
                message: format!(
                    "No lane has {ADVISE_MIN_DECIDED} finished (success or failure) jobs yet; \
                     {total} decided across {} lane(s).",
                    lanes.len()
                ),
                sample_count: total,
                suggested_poll_interval_secs: 600,
                recommended_actions: vec![
                    "Keep collecting metrics before changing profiles.".to_owned(),
                ],
                basis: proxy::Basis::WallTime.as_str(),
                comparison: None,
                denominator: None,
                gate: None,
            });
        } else {
            findings.push(MetricsFinding {
                severity: "watch".to_owned(),
                lane: target,
                signal: "no_healthy_lane".to_owned(),
                message: format!(
                    "Every sampled lane fails more than {:.0}% of decided jobs ({}); \
                     {total} decided jobs in all.",
                    ADVISE_MAX_FAILURE_RATE * 100.0,
                    sampled.join(", ")
                ),
                sample_count: total,
                suggested_poll_interval_secs: 600,
                recommended_actions: vec![
                    "Placement is not the lever: fix the failures before moving the job."
                        .to_owned(),
                ],
                basis: proxy::Basis::WallTime.as_str(),
                comparison: None,
                denominator: None,
                gate: None,
            });
        }
    }
    findings
}

fn compare_findings(rows: Vec<SummaryInput>, split_days_ago: i64) -> Vec<MetricsFinding> {
    let split = Utc::now() - Duration::days(split_days_ago.max(1));
    let mut groups: BTreeMap<String, (Vec<i64>, Vec<i64>)> = BTreeMap::new();
    for row in rows {
        if !(row.status == "pass" || row.status == "success") {
            continue;
        }
        let Some(completed_at) = row.completed_at else {
            continue;
        };
        let Some(total_ms) = row.total_ms else {
            continue;
        };
        let lane = format!("{}/{}/{}", row.target, row.backend, row.host);
        if completed_at < split {
            groups.entry(lane).or_default().0.push(total_ms);
        } else {
            groups.entry(lane).or_default().1.push(total_ms);
        }
    }
    let mut findings = Vec::new();
    for (lane, (mut before, mut after)) in groups {
        before.sort_unstable();
        after.sort_unstable();
        if before.is_empty() || after.is_empty() {
            continue;
        }
        let before_p50 = percentile(&before, 50).unwrap_or_default();
        let after_p50 = percentile(&after, 50).unwrap_or_default();
        let severity = if before_p50 > 0 && after_p50 * 100 <= before_p50 * 80 {
            "optimize"
        } else if before_p50 > 0 && after_p50 * 100 >= before_p50 * 125 {
            "investigate"
        } else {
            "info"
        };
        findings.push(MetricsFinding {
            severity: severity.to_owned(),
            lane,
            signal: "p50_total_ms_compare".to_owned(),
            message: format!(
                "p50 changed from {before_p50}ms before split to {after_p50}ms after split."
            ),
            sample_count: before.len() + after.len(),
            suggested_poll_interval_secs: 600,
            recommended_actions: vec![
                "Inspect profile, cache, and runner image changes near the split.".to_owned(),
            ],
            basis: proxy::Basis::WallTime.as_str(),
            comparison: None,
            denominator: None,
            gate: None,
        });
    }
    findings
}

#[allow(clippy::cast_precision_loss)]
fn failure_rate(failures: usize, count: usize) -> f64 {
    if count == 0 {
        0.0
    } else {
        failures as f64 / count as f64
    }
}

fn percentile(values: &[i64], percentile: usize) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let span = values.len() - 1;
    let index = (span * percentile).div_ceil(100);
    values.get(index).copied()
}

fn average(values: &[i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<i64>() / i64::try_from(values.len()).ok()?)
}

fn value_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn value_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.is_empty())
}

/// Parse duration flags accepted by `shipyard metrics record`.
#[allow(clippy::cast_possible_truncation)]
pub fn parse_duration_ms(value: &str) -> Result<i64, String> {
    let trimmed = value.trim();
    if let Some(ms) = trimmed.strip_suffix("ms") {
        return ms
            .parse::<i64>()
            .map_err(|_| format!("invalid millisecond duration: {value}"));
    }
    if let Some(secs) = trimmed.strip_suffix('s') {
        let parsed = secs
            .parse::<f64>()
            .map_err(|_| format!("invalid second duration: {value}"))?;
        return Ok((parsed * 1000.0).round() as i64);
    }
    trimmed
        .parse::<i64>()
        .map_err(|_| format!("invalid duration: {value}"))
}

#[derive(Debug, Deserialize)]
pub struct GitHubRunJob {
    pub id: i64,
    pub run_id: Option<i64>,
    pub run_attempt: Option<i64>,
    pub name: String,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub runner_name: Option<String>,
    pub runner_group_name: Option<String>,
    pub labels: Option<Vec<String>>,
    pub created_at: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

/// Import one GitHub Actions job object as returned by `/actions/runs/{id}/jobs`.
pub fn github_job_to_record(
    repo: &str,
    workflow: Option<&str>,
    project: &str,
    pr: Option<i64>,
    job: &GitHubRunJob,
) -> MetricRecordInput {
    let started_at = job
        .started_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    let queued_at = job
        .created_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    let completed_at = job
        .completed_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    let duration_ms = started_at
        .zip(completed_at)
        .map(|(started, completed)| (completed - started).num_milliseconds())
        .unwrap_or_default();
    let provider = if job
        .labels
        .as_ref()
        .is_some_and(|labels| labels.iter().any(|label| label == "self-hosted"))
    {
        "self-hosted"
    } else {
        "github-hosted"
    };
    MetricRecordInput {
        project: project.to_owned(),
        repo: Some(repo.to_owned()),
        pr,
        workflow: workflow.map(str::to_owned),
        job: job.name.clone(),
        target: Some(job.name.clone()),
        backend: Some(
            if provider == "self-hosted" {
                "local"
            } else {
                "cloud"
            }
            .to_owned(),
        ),
        provider: Some(provider.to_owned()),
        runner: job.runner_name.clone(),
        host: job.runner_name.clone().or(job.runner_group_name.clone()),
        step: Some("github_job".to_owned()),
        duration_ms,
        status: job
            .conclusion
            .clone()
            .or(job.status.clone())
            .unwrap_or_else(|| "unknown".to_owned()),
        external_id: Some(format!(
            "github:{}/{}/{}",
            job.run_id.unwrap_or_default(),
            job.id,
            job.run_attempt.unwrap_or(1)
        )),
        started_at,
        queued_at,
        completed_at,
        runner_assigned: Some(
            job.runner_name
                .as_deref()
                .is_some_and(|name| !name.trim().is_empty()),
        ),
        labels: job.labels.clone(),
        ..MetricRecordInput::default()
    }
}

#[cfg(test)]
mod instrument_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_scorecard_sample(
        store: &MetricsStore,
        project: &str,
        pr: i64,
        status: &str,
        total_ms: i64,
        queue_ms: i64,
        cache_hit: bool,
    ) {
        insert_scorecard_sample_at(
            store,
            project,
            pr,
            status,
            total_ms,
            queue_ms,
            cache_hit,
            Utc::now().to_rfc3339(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_scorecard_sample_at(
        store: &MetricsStore,
        project: &str,
        pr: i64,
        status: &str,
        total_ms: i64,
        queue_ms: i64,
        cache_hit: bool,
        completed_at: String,
    ) {
        let conn = store.connect().expect("metrics connection");
        let run_id = insert_run(
            &conn,
            &RunInsert {
                ts: completed_at.clone(),
                project: project.to_owned(),
                repo: Some("danielraffel/Shipyard".to_owned()),
                branch: Some("main".to_owned()),
                sha: Some(format!("head-{pr}")),
                pr: Some(pr),
                workflow: Some("Build and Test".to_owned()),
                profile: None,
                routing_decision: None,
                status: status.to_owned(),
            },
        )
        .expect("insert run");
        let job_id = insert_job(
            &conn,
            &JobInsert {
                run_id,
                machine_id: None,
                job: format!("job-{pr}-{total_ms}"),
                target: Some("test".to_owned()),
                platform: Some("macos".to_owned()),
                backend: Some("vm".to_owned()),
                provider: Some("tart-macos".to_owned()),
                queued_at: None,
                started_at: None,
                completed_at: Some(completed_at),
                queue_ms: Some(queue_ms),
                boot_ms: None,
                setup_ms: None,
                run_ms: Some(total_ms),
                total_ms: Some(total_ms),
                status: status.to_owned(),
                exit_code: None,
                failure_class: None,
                external_id: None,
                runner_assigned: None,
                labels_json: None,
            },
        )
        .expect("insert job");
        conn.execute(
            "INSERT INTO steps (job_id, step, duration_ms, status, cache_hit) VALUES (?1, 'build', ?2, ?3, ?4)",
            params![job_id, total_ms, status, cache_hit],
        )
        .expect("insert cache sample");
    }

    #[test]
    fn record_and_summary_round_trip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        store
            .record(&MetricRecordInput {
                project: "pulp".to_owned(),
                job: "linux-arm64".to_owned(),
                target: Some("linux-arm64".to_owned()),
                backend: Some("local".to_owned()),
                provider: Some("tart-linux".to_owned()),
                host: Some("macstudio".to_owned()),
                step: Some("compile".to_owned()),
                duration_ms: 1200,
                status: "pass".to_owned(),
                ..MetricRecordInput::default()
            })
            .expect("record");
        let summary = store.summary(Some("pulp")).expect("summary");
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].p50_ms, Some(1200));
        assert_eq!(summary[0].host, "macstudio");
    }

    #[test]
    fn imports_tartci_jsonl_and_deduplicates_external_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        let row = r#"{"project":"pulp","repo":"danielraffel/pulp","provider":"tart-macos","backend":"vm","host":"macstudio","platform":"macos","arch":"arm64","job":"Coverage report (macOS, Clang)","target":"coverage-macos","status":"pass","total_ms":180000,"completed_at":"2026-06-12T10:00:00Z","external_id":"github:1/2/1","phases_ms":{"boot_to_ssh":10000,"runner_process":170000}}"#;
        assert_eq!(store.import_tartci(row).expect("import"), 1);
        assert_eq!(store.import_tartci(row).expect("dedupe"), 0);
        let rows = store.list(Some("pulp"), 10).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider.as_deref(), Some("tart-macos"));
    }

    #[test]
    fn parses_duration_units() {
        assert_eq!(parse_duration_ms("42"), Ok(42));
        assert_eq!(parse_duration_ms("42ms"), Ok(42));
        assert_eq!(parse_duration_ms("1.5s"), Ok(1500));
    }

    #[test]
    fn stewardship_scorecard_reports_empty_store_without_invented_telemetry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");

        let scorecard = store
            .stewardship_scorecard("shipyard", 14)
            .expect("scorecard");

        assert_eq!(scorecard.job_samples, 0);
        assert_eq!(scorecard.failure_rate, None);
        assert!(scorecard.worker_minutes.abs() < f64::EPSILON);
        assert_eq!(scorecard.duration_samples, 0);
        assert_eq!(scorecard.worker_minutes_coverage.status, "unavailable");
        assert_eq!(scorecard.duration_p50_ms, None);
        assert_eq!(scorecard.queue_p90_ms, None);
        assert_eq!(scorecard.cache_hit_rate, None);
        assert_eq!(scorecard.pull_request_throughput.status, "unavailable");
        assert_eq!(scorecard.submit_to_receipt.status, "unavailable");
        assert_eq!(scorecard.model_token_use.status, "unavailable");
    }

    #[test]
    fn stewardship_scorecard_aggregates_work_queue_pr_and_cache_history() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        // Insert out of duration and queue order: percentile calculation must
        // not depend on row or completion order.
        insert_scorecard_sample(&store, "shipyard", 41, "pass", 180_000, 1_000, true);
        insert_scorecard_sample(&store, "shipyard", 41, "success", 60_000, 3_000, true);
        insert_scorecard_sample(&store, "shipyard", 42, "failure", 120_000, 2_000, false);
        insert_scorecard_sample(&store, "other", 99, "failure", 9_000_000, 90_000, false);

        let scorecard = store
            .stewardship_scorecard("shipyard", 2)
            .expect("scorecard");

        assert_eq!(scorecard.job_samples, 3);
        assert_eq!(scorecard.successful_jobs, 2);
        assert_eq!(scorecard.failed_jobs, 1);
        assert_eq!(scorecard.other_jobs, 0);
        assert_eq!(scorecard.failure_rate, Some(1.0 / 3.0));
        assert!((scorecard.worker_minutes - 6.0).abs() < f64::EPSILON);
        assert_eq!(scorecard.duration_samples, 3);
        assert_eq!(scorecard.worker_minutes_coverage.status, "available");
        assert_eq!(scorecard.duration_p50_ms, Some(120_000));
        assert_eq!(scorecard.duration_p90_ms, Some(180_000));
        assert_eq!(scorecard.queue_samples, 3);
        assert_eq!(scorecard.queue_p50_ms, Some(2_000));
        assert_eq!(scorecard.queue_p90_ms, Some(3_000));
        assert_eq!(scorecard.distinct_pull_requests, 2);
        assert!((scorecard.pull_requests_per_day - 1.0).abs() < f64::EPSILON);
        assert_eq!(scorecard.pull_request_throughput.status, "available");
        assert_eq!(scorecard.cache_samples, 3);
        assert_eq!(scorecard.cache_hits, 2);
        assert_eq!(scorecard.cache_hit_rate, Some(2.0 / 3.0));
    }

    #[test]
    fn stewardship_scorecard_separates_nonfailure_outcomes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        insert_scorecard_sample(&store, "shipyard", 41, "success", 60_000, 1_000, true);
        insert_scorecard_sample(&store, "shipyard", 42, "failure", 60_000, 1_000, false);
        insert_scorecard_sample(&store, "shipyard", 43, "cancelled", 60_000, 1_000, false);
        insert_scorecard_sample(&store, "shipyard", 44, "skipped", 60_000, 1_000, false);
        insert_scorecard_sample(&store, "shipyard", 45, "neutral", 60_000, 1_000, false);

        let scorecard = store
            .stewardship_scorecard("shipyard", 2)
            .expect("scorecard");

        assert_eq!(scorecard.job_samples, 5);
        assert_eq!(scorecard.successful_jobs, 1);
        assert_eq!(scorecard.failed_jobs, 1);
        assert_eq!(scorecard.other_jobs, 3);
        assert_eq!(scorecard.failure_rate, Some(0.5));
    }

    #[test]
    fn stewardship_scorecard_compares_offset_timestamps_chronologically() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        let within_window = Utc::now() - Duration::hours(23);
        let pacific = chrono::FixedOffset::west_opt(7 * 60 * 60).expect("fixed offset");
        insert_scorecard_sample_at(
            &store,
            "shipyard",
            41,
            "success",
            60_000,
            1_000,
            true,
            within_window.with_timezone(&pacific).to_rfc3339(),
        );

        let scorecard = store
            .stewardship_scorecard("shipyard", 1)
            .expect("scorecard");

        assert_eq!(scorecard.job_samples, 1);
        assert_eq!(scorecard.cache_samples, 1);
    }

    #[test]
    fn stewardship_scorecard_rejects_ambiguous_scope() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");

        assert!(store.stewardship_scorecard("", 14).is_err());
        assert!(store.stewardship_scorecard("shipyard", 0).is_err());
        assert!(store.stewardship_scorecard("shipyard", -1).is_err());
        assert!(store.stewardship_scorecard("shipyard", i64::MAX).is_err());
    }

    #[test]
    fn stewardship_scorecard_marks_incomplete_pr_identity_coverage() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        insert_scorecard_sample(&store, "shipyard", 41, "success", 60_000, 1_000, true);
        insert_scorecard_sample(&store, "shipyard", 42, "success", 60_000, 1_000, true);
        let conn = store.connect().expect("metrics connection");
        conn.execute("UPDATE runs SET pr = NULL WHERE pr = 42", [])
            .expect("remove one PR identity");

        let scorecard = store
            .stewardship_scorecard("shipyard", 2)
            .expect("scorecard");

        assert_eq!(scorecard.distinct_pull_requests, 1);
        assert_eq!(scorecard.pull_request_throughput.status, "partial");
        assert!(
            scorecard
                .pull_request_throughput
                .reason
                .starts_with("1 of 2")
        );
    }

    #[test]
    fn github_job_record_preserves_workflow_run_pr_identity() {
        let job = GitHubRunJob {
            id: 7,
            run_id: Some(8),
            run_attempt: Some(1),
            name: "Build".to_owned(),
            status: Some("completed".to_owned()),
            conclusion: Some("success".to_owned()),
            runner_name: None,
            runner_group_name: None,
            labels: None,
            created_at: Some("2026-09-01T11:59:30Z".to_owned()),
            started_at: Some("2026-09-01T12:00:00Z".to_owned()),
            completed_at: Some("2026-09-01T12:01:00Z".to_owned()),
        };

        let record = github_job_to_record(
            "danielraffel/Shipyard",
            Some("build.yml"),
            "Shipyard",
            Some(538),
            &job,
        );

        assert_eq!(record.pr, Some(538));
        assert_eq!(
            record.queued_at,
            Some(
                DateTime::parse_from_rfc3339("2026-09-01T11:59:30Z")
                    .unwrap()
                    .with_timezone(&Utc),
            )
        );
    }

    #[test]
    fn github_job_queue_timing_is_persisted_only_from_authoritative_timestamps() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        let job = GitHubRunJob {
            id: 9,
            run_id: Some(10),
            run_attempt: Some(1),
            name: "Build".to_owned(),
            status: Some("completed".to_owned()),
            conclusion: Some("success".to_owned()),
            runner_name: None,
            runner_group_name: None,
            labels: None,
            created_at: Some("2026-09-01T11:59:30Z".to_owned()),
            started_at: Some("2026-09-01T12:00:00Z".to_owned()),
            completed_at: Some("2026-09-01T12:01:00Z".to_owned()),
        };
        let record = github_job_to_record("owner/repo", None, "repo", None, &job);
        store.record(&record).expect("record");
        let conn = store.connect().expect("connection");
        let (queued_at, queue_ms): (String, i64) = conn
            .query_row("SELECT queued_at, queue_ms FROM jobs", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("queue timing");
        assert_eq!(queued_at, "2026-09-01T11:59:30+00:00");
        assert_eq!(queue_ms, 30_000);
    }

    #[test]
    fn duplicate_external_job_refreshes_terminal_state_and_pr_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        let external_id = Some("github:8/7/1".to_owned());
        let first_id = store
            .record(&MetricRecordInput {
                project: "placeholder".to_owned(),
                job: "Build".to_owned(),
                provider: Some("github-hosted".to_owned()),
                status: "in_progress".to_owned(),
                external_id: external_id.clone(),
                ..MetricRecordInput::default()
            })
            .expect("initial job");
        let refreshed_id = store
            .record_terminal_observation(&MetricRecordInput {
                project: "shipyard".to_owned(),
                repo: Some("danielraffel/Shipyard".to_owned()),
                pr: Some(538),
                job: "Build".to_owned(),
                provider: Some("github-hosted".to_owned()),
                runner: Some("mac-runner".to_owned()),
                host: Some("mac-runner".to_owned()),
                step: Some("github_job".to_owned()),
                duration_ms: 60_000,
                status: "success".to_owned(),
                external_id,
                queued_at: Some(
                    DateTime::parse_from_rfc3339("2026-09-01T11:59:30Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                started_at: Some(
                    DateTime::parse_from_rfc3339("2026-09-01T12:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                completed_at: Some(Utc::now()),
                ..MetricRecordInput::default()
            })
            .expect("refreshed job");

        assert_eq!(refreshed_id, first_id);
        let scorecard = store
            .stewardship_scorecard("shipyard", 1)
            .expect("scorecard");
        assert_eq!(scorecard.job_samples, 1);
        assert_eq!(scorecard.successful_jobs, 1);
        assert_eq!(scorecard.distinct_pull_requests, 1);
        assert_eq!(scorecard.pull_request_throughput.status, "available");
        let conn = store.connect().expect("metrics connection");
        let machine: String = conn
            .query_row(
                "SELECT machine.name FROM jobs job JOIN machines machine ON machine.id = job.machine_id WHERE job.id = ?1",
                params![refreshed_id],
                |row| row.get(0),
            )
            .expect("refreshed machine");
        assert_eq!(machine, "mac-runner");
        let queue_ms: i64 = conn
            .query_row(
                "SELECT queue_ms FROM jobs WHERE id = ?1",
                params![refreshed_id],
                |row| row.get(0),
            )
            .expect("refreshed queue timing");
        assert_eq!(queue_ms, 30_000);
        let step: (String, String, i64) = conn
            .query_row(
                "SELECT step, status, duration_ms FROM steps WHERE job_id = ?1",
                params![refreshed_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("refreshed step");
        assert_eq!(
            step,
            ("github_job".to_owned(), "success".to_owned(), 60_000)
        );
    }

    #[test]
    fn ordinary_duplicate_record_does_not_rewrite_terminal_history() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        let external_id = Some("github:8/7/1".to_owned());
        let completed_at = Utc::now() - Duration::days(30);
        let first_id = store
            .record(&MetricRecordInput {
                project: "shipyard".to_owned(),
                job: "Build".to_owned(),
                provider: Some("github-hosted".to_owned()),
                duration_ms: 60_000,
                status: "success".to_owned(),
                external_id: external_id.clone(),
                completed_at: Some(completed_at),
                ..MetricRecordInput::default()
            })
            .expect("initial job");
        let duplicate_id = store
            .record(&MetricRecordInput {
                project: "shipyard".to_owned(),
                job: "Build".to_owned(),
                provider: Some("github-hosted".to_owned()),
                status: "in_progress".to_owned(),
                external_id,
                ..MetricRecordInput::default()
            })
            .expect("duplicate job");

        assert_eq!(duplicate_id, first_id);
        assert_eq!(
            store
                .stewardship_scorecard("shipyard", 1)
                .expect("scorecard")
                .job_samples,
            0
        );
    }

    #[test]
    fn scorecard_marks_worker_minutes_partial_when_a_duration_is_unmeasured() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        store
            .record_terminal_observation(&MetricRecordInput {
                project: "shipyard".to_owned(),
                repo: Some("danielraffel/Shipyard".to_owned()),
                pr: Some(538),
                job: "Conditional job".to_owned(),
                provider: Some("github-hosted".to_owned()),
                step: Some("github_job".to_owned()),
                status: "skipped".to_owned(),
                completed_at: Some(Utc::now()),
                external_id: Some("github:8/9/1".to_owned()),
                ..MetricRecordInput::default()
            })
            .expect("terminal job");
        insert_scorecard_sample(&store, "shipyard", 539, "success", 60_000, 1_000, true);

        let scorecard = store
            .stewardship_scorecard("shipyard", 1)
            .expect("scorecard");

        assert_eq!(scorecard.job_samples, 2);
        assert_eq!(scorecard.other_jobs, 1);
        assert!((scorecard.worker_minutes - 1.0).abs() < f64::EPSILON);
        assert_eq!(scorecard.duration_samples, 1);
        assert_eq!(scorecard.duration_p50_ms, Some(60_000));
        assert_eq!(scorecard.worker_minutes_coverage.status, "partial");
    }

    #[test]
    fn stewardship_scorecard_scopes_pr_identity_by_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        insert_scorecard_sample(&store, "shipyard", 42, "success", 60_000, 1_000, true);
        insert_scorecard_sample(&store, "shipyard", 42, "success", 60_000, 1_000, true);
        let conn = store.connect().expect("metrics connection");
        conn.execute(
            "UPDATE runs SET repo = 'another/repo' WHERE id = (SELECT MAX(id) FROM runs)",
            [],
        )
        .expect("change second repository");

        let scorecard = store
            .stewardship_scorecard("shipyard", 1)
            .expect("scorecard");

        assert_eq!(scorecard.distinct_pull_requests, 2);
        assert_eq!(scorecard.pull_request_throughput.status, "available");
    }

    #[test]
    fn stewardship_scorecard_worker_minutes_cannot_overflow_i64() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        insert_scorecard_sample(&store, "shipyard", 41, "success", i64::MAX, 1_000, true);
        insert_scorecard_sample(&store, "shipyard", 42, "success", i64::MAX, 1_000, true);

        let scorecard = store
            .stewardship_scorecard("shipyard", 1)
            .expect("scorecard");

        assert!(scorecard.worker_minutes.is_finite());
        assert!(scorecard.worker_minutes > worker_minutes(&[i64::MAX]));
    }
}
