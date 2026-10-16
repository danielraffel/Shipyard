#![allow(missing_docs)]

pub mod freshness;
pub mod github_import;
pub mod import_schedule;
pub mod job_name;
pub mod merge_receipt;
pub mod project_key;
pub mod proxy;
mod proxy_store;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use project_key::ProjectKey;

/// Durable Shipyard metrics store backed by `SQLite`.
#[derive(Debug)]
pub struct MetricsStore {
    path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct ExistingMergeReceipt {
    schema: String,
    repo: String,
    project: String,
    base_branch: String,
    pr: i64,
    head_sha: String,
    validated_head_sha: String,
    merge_commit_sha: String,
    merged_at: String,
    source: String,
    required_checks_json: String,
    gate_receipt_sha256: String,
    evidence: String,
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
    /// Stable identity shared by all observations and phase rows for one job.
    /// When omitted, a provider/external-id pair supplies a deterministic
    /// fallback; observations without either identity remain uncorrelated.
    pub correlation_id: Option<String>,
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
    /// GitHub workflow event that produced the run, when authoritative.
    pub event: Option<String>,
    /// Provider-owned workflow run identity, distinct from the local `SQLite`
    /// run row id.
    pub provider_run_id: Option<i64>,
    /// Provider-owned attempt number for the workflow run.
    pub run_attempt: Option<i64>,
    /// Authoritative commit built by the provider. This is kept separately
    /// from the caller's generic `sha` so a missing provider field stays
    /// unknown rather than being guessed from a branch or PR.
    pub head_sha: Option<String>,
    /// Authenticated provenance label. Absent means unknown and is excluded
    /// from promotion-oriented scorecard populations.
    pub provenance: Option<String>,
    /// Internal qualification bit; caller-provided provenance text is never
    /// eligible for scorecard promotion unless an authenticated importer sets
    /// this explicitly.
    pub provenance_verified: bool,
}

/// Shadow-only classification for queue work that the current enqueue policy
/// would supersede or repeat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepeatedWorkKind {
    /// A pending job for the same queue scope has an older exact head.
    SupersededExactHead,
    /// A pending job already has the same exact head and queue scope.
    DuplicateExactHeadConfig,
}

impl RepeatedWorkKind {
    /// Stable storage label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SupersededExactHead => "superseded_exact_head",
            Self::DuplicateExactHeadConfig => "duplicate_exact_head_config",
        }
    }
}

/// One queue repeated-work observation. This is accounting only; it does not
/// authorize skipping or alter queue state transitions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepeatedWorkShadowInput {
    pub kind: RepeatedWorkKind,
    pub branch: String,
    pub sha: String,
    pub target_config: String,
    pub mode: String,
    pub job_id: String,
    pub superseded_job_id: String,
    pub superseded_sha: String,
}

/// Persisted repeated-work observation returned by the read-side query.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepeatedWorkShadowRow {
    pub observed_at: String,
    pub kind: String,
    pub branch: String,
    pub sha: String,
    pub target_config: String,
    pub mode: String,
    pub job_id: String,
    pub superseded_job_id: String,
    pub superseded_sha: String,
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
    pub correlation_id: Option<String>,
}

/// One persisted phase measurement for a correlated job.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MetricPhaseRow {
    pub correlation_id: Option<String>,
    pub phase: String,
    pub status: String,
    pub duration_ms: i64,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
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
    /// Wall-clock compute minutes spent on failed or cancelled jobs. This is a
    /// cost proxy: a cancelled or failed attempt may be retried.
    pub wasted_compute_minutes: f64,
    /// Number of failed or cancelled jobs contributing to
    /// [`Self::wasted_compute_minutes`].
    pub wasted_compute_samples: usize,
    /// Queue wait minutes with an authoritative queued timestamp.
    /// This is the coordination-cost proxy.
    pub coordination_minutes: f64,
    /// Number of jobs contributing to [`Self::coordination_minutes`].
    pub coordination_samples: usize,
    /// Failed-attempt compute minutes. Cancellations are kept in
    /// `wasted_compute_minutes` but are not called rework without evidence
    /// that a replacement attempt ran.
    pub rework_compute_minutes: f64,
    /// Number of failed attempts contributing to rework compute.
    pub rework_compute_samples: usize,
    /// Sum of measured queue and failed-attempt compute minutes.
    pub coordination_rework_minutes: f64,
    /// Merge receipts are not currently part of the local metrics schema.
    /// Keep this explicit instead of dividing by distinct PRs and calling
    /// those changes safely merged.
    pub safely_merged_changes: ScorecardCoverage,
    /// Wasted compute per safely merged change; unavailable until merge
    /// receipts are imported.
    pub wasted_compute_per_safely_merged_change: ScorecardCoverage,
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
    /// Counts of completed samples by authenticated provenance. Unknown rows
    /// are retained for audit but never promoted into a provenance-qualified
    /// population.
    pub provenance: ScorecardProvenance,
    pub strata: ScorecardStrata,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ScorecardProvenance {
    pub production: usize,
    pub synthetic: usize,
    pub unknown: usize,
    pub eligible_samples: usize,
    pub status: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ScorecardStrata {
    pub normal_pr: usize,
    pub merge_group: usize,
    pub rerun: usize,
    pub unclassified: usize,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScorecardCoverage {
    pub status: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
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

    #[allow(clippy::too_many_lines)]
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
              status TEXT NOT NULL,
              event TEXT,
              provider_run_id INTEGER,
              run_attempt INTEGER,
              head_sha TEXT,
              provenance TEXT,
              provenance_verified INTEGER NOT NULL DEFAULT 0
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
              correlation_id TEXT,
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
            CREATE TABLE IF NOT EXISTS merge_receipts (
              receipt_id TEXT PRIMARY KEY,
              schema TEXT NOT NULL,
              repo TEXT NOT NULL,
              project TEXT NOT NULL,
              base_branch TEXT NOT NULL,
              pr INTEGER NOT NULL,
              head_sha TEXT NOT NULL,
              validated_head_sha TEXT NOT NULL,
              merge_commit_sha TEXT NOT NULL,
              merged_at TEXT NOT NULL,
              safe_merge INTEGER NOT NULL,
              source TEXT NOT NULL,
              required_checks_json TEXT NOT NULL,
              gate_receipt_sha256 TEXT NOT NULL,
              evidence TEXT NOT NULL,
              imported_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS merge_receipts_project_merged
              ON merge_receipts(project, merged_at);
            CREATE TABLE IF NOT EXISTS repeated_work_shadow (
              id INTEGER PRIMARY KEY,
              observed_at TEXT NOT NULL,
              kind TEXT NOT NULL,
              branch TEXT NOT NULL,
              sha TEXT NOT NULL,
              target_config TEXT NOT NULL,
              mode TEXT NOT NULL,
              job_id TEXT NOT NULL,
              superseded_job_id TEXT NOT NULL,
              superseded_sha TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS repeated_work_shadow_observed_at
              ON repeated_work_shadow(observed_at DESC);
            ",
        )?;
        add_column_if_missing(&conn, "jobs", "runner_assigned", "INTEGER")?;
        add_column_if_missing(&conn, "jobs", "labels_json", "TEXT")?;
        add_column_if_missing(&conn, "jobs", "correlation_id", "TEXT")?;
        add_column_if_missing(&conn, "runs", "event", "TEXT")?;
        add_column_if_missing(&conn, "runs", "provider_run_id", "INTEGER")?;
        add_column_if_missing(&conn, "runs", "run_attempt", "INTEGER")?;
        add_column_if_missing(&conn, "runs", "head_sha", "TEXT")?;
        add_column_if_missing(&conn, "runs", "provenance", "TEXT")?;
        add_column_if_missing(
            &conn,
            "runs",
            "provenance_verified",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(
            &conn,
            "merge_receipts",
            "required_checks_json",
            "TEXT NOT NULL DEFAULT '[]'",
        )?;
        add_column_if_missing(
            &conn,
            "merge_receipts",
            "gate_receipt_sha256",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        // Older stores only had the provider/external-id identity. Backfill
        // the new stable correlation key without changing any row identity.
        conn.execute(
            "UPDATE jobs
                SET correlation_id = provider || ':' || external_id
              WHERE correlation_id IS NULL
                AND provider IS NOT NULL AND external_id IS NOT NULL
                AND provider <> '' AND external_id <> ''",
            [],
        )?;
        Ok(())
    }

    /// Import validated merge receipts atomically. Duplicate receipt IDs are
    /// idempotent only when the payload is byte-for-byte equivalent.
    pub fn import_merge_receipts(
        &self,
        receipts: &[crate::metrics::merge_receipt::MergeReceipt],
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut imported = 0;
        for receipt in receipts {
            receipt
                .validate()
                .map_err(|e| format!("invalid merge receipt: {e}"))?;
            // Repository and project are durable ownership keys. Normalize
            // them before both conflict checks and storage so case/outer
            // whitespace cannot create a second identity for one PR.
            let repo = crate::evidence::canonical_repository(&receipt.repo);
            let project_input = crate::evidence::canonical_repository(&receipt.project);
            let (project, _) =
                crate::metrics::project_key::normalize_for_write(&project_input, Some(&repo));
            let existing: Option<ExistingMergeReceipt> = tx.query_row(
                "SELECT schema,repo,project,base_branch,pr,head_sha,validated_head_sha,merge_commit_sha,merged_at,source,required_checks_json,gate_receipt_sha256,evidence FROM merge_receipts WHERE receipt_id = ?1",
                [&receipt.receipt_id],
                |row| Ok(ExistingMergeReceipt {
                    schema: row.get(0)?, repo: row.get(1)?, project: row.get(2)?, base_branch: row.get(3)?, pr: row.get(4)?,
                    head_sha: row.get(5)?, validated_head_sha: row.get(6)?, merge_commit_sha: row.get(7)?, merged_at: row.get(8)?,
                    source: row.get(9)?, required_checks_json: row.get(10)?, gate_receipt_sha256: row.get(11)?, evidence: row.get(12)?,
                }),
            ).optional()?;
            if let Some(existing) = existing {
                let incoming = ExistingMergeReceipt {
                    schema: receipt.schema.clone(),
                    repo: repo.clone(),
                    project: project.clone(),
                    base_branch: receipt.base_branch.clone(),
                    pr: receipt.pr,
                    head_sha: receipt.head_sha.clone(),
                    validated_head_sha: receipt.validated_head_sha.clone(),
                    merge_commit_sha: receipt.merge_commit_sha.clone(),
                    merged_at: receipt.merged_at.to_rfc3339(),
                    source: receipt.source.clone(),
                    required_checks_json: serde_json::to_string(&receipt.required_checks)?,
                    gate_receipt_sha256: receipt.gate_receipt_sha256.clone(),
                    evidence: receipt.evidence.clone(),
                };
                if existing != incoming {
                    return Err(format!(
                        "merge receipt {} conflicts with existing payload",
                        receipt.receipt_id
                    )
                    .into());
                }
                continue;
            }
            let semantic_conflict: Option<String> = tx.query_row(
                "SELECT receipt_id FROM merge_receipts WHERE lower(trim(repo)) = ?1 AND pr = ?2 AND receipt_id <> ?3 AND (lower(trim(project)) <> ?4 OR base_branch <> ?5 OR head_sha <> ?6 OR merge_commit_sha <> ?7)",
                rusqlite::params![repo, receipt.pr, receipt.receipt_id, project, receipt.base_branch, receipt.head_sha, receipt.merge_commit_sha],
                |row| row.get(0),
            ).optional()?;
            if let Some(other) = semantic_conflict {
                return Err(format!(
                    "merge receipt {} conflicts with semantic receipt {other}",
                    receipt.receipt_id
                )
                .into());
            }
            tx.execute("INSERT INTO merge_receipts (receipt_id,schema,repo,project,base_branch,pr,head_sha,validated_head_sha,merge_commit_sha,merged_at,safe_merge,source,required_checks_json,gate_receipt_sha256,evidence,imported_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1,?11,?12,?13,?14,?15)", rusqlite::params![receipt.receipt_id, receipt.schema, repo, project, receipt.base_branch, receipt.pr, receipt.head_sha, receipt.validated_head_sha, receipt.merge_commit_sha, receipt.merged_at.to_rfc3339(), receipt.source, serde_json::to_string(&receipt.required_checks)?, receipt.gate_receipt_sha256, receipt.evidence, Utc::now().to_rfc3339()])?;
            imported += 1;
        }
        tx.commit()?;
        Ok(imported)
    }

    /// Count safe merged heads and the failed/cancelled compute attached to
    /// those exact PR identities in the scorecard window.
    pub(crate) fn safe_merge_summary(
        &self,
        project: &str,
        cutoff: &str,
    ) -> Result<(usize, usize, i64, usize, usize), Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        let key = crate::metrics::project_key::ProjectKey::parse(project);
        let (any, short, full) = crate::metrics::project_key::sql_params(key.as_ref());
        let project_filter = crate::metrics::project_key::sql_filter("project", "repo");
        let safe: Vec<(String, String, i64)> = {
            let mut q = conn.prepare(&format!("SELECT repo, head_sha, pr FROM merge_receipts WHERE {project_filter} AND safe_merge = 1 AND required_checks_json <> '[]' AND length(gate_receipt_sha256) = 64 AND julianday(merged_at) >= julianday(?4)"))?;
            q.query_map(rusqlite::params![any, short, full, cutoff], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<_, _>>()?
        };
        let unique = safe.into_iter().collect::<std::collections::BTreeSet<_>>();
        let total = unique.len();
        let mut joined = 0_usize;
        let mut wasted = 0_i64;
        let mut samples = 0_usize;
        let mut duration_gaps = 0_usize;
        for (repo, head, pr) in unique {
            let mut q = conn.prepare("SELECT job.status, job.total_ms FROM jobs job JOIN runs run ON run.id = job.run_id WHERE lower(trim(run.repo)) = ?1 AND run.sha = ?2 AND run.pr = ?3 AND job.completed_at IS NOT NULL")?;
            let mut matched = false;
            let mut invalid_duration = false;
            for row in q.query_map(rusqlite::params![repo, head, pr], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
            })? {
                let (_, ms) = row?;
                if ms.is_none_or(|v| v < 0) {
                    // A completed row with NULL/negative duration cannot
                    // substantiate a cost join. Keep the merge unjoined so
                    // the scorecard cannot report a zero-cost claim.
                    invalid_duration = true;
                    continue;
                }
                matched = true;
            }
            if matched && !invalid_duration {
                // The merged head proved the join; the change's waste is every
                // failed or cancelled job of its pull request, earlier pushes
                // included, not only the jobs on the head that merged.
                let (pr_wasted, pr_samples, pr_duration_gaps) =
                    pull_request_waste(&conn, &repo, pr)?;
                joined += 1;
                wasted = wasted.saturating_add(pr_wasted);
                samples += pr_samples;
                duration_gaps = duration_gaps.saturating_add(pr_duration_gaps);
            }
        }
        Ok((total, joined, wasted, samples, duration_gaps))
    }

    /// Record a queue repeated-work observation without affecting queue
    /// admission, dispatch, or required-check authority.
    pub fn record_repeated_work_shadow(
        &self,
        input: &RepeatedWorkShadowInput,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        let conn = self.connect()?;
        Ok(Self::insert_repeated_work_shadow(&conn, input)?)
    }

    /// Record one shadow observation without waiting for a sandbox writer
    /// lease. Returns `false` when `SQLite` reports a busy/locked database;
    /// writer-domain lease contention is returned as an error, which callers
    /// may also safely drop for this advisory observation.
    pub(crate) fn record_repeated_work_shadow_best_effort(
        state_dir: &Path,
        input: &RepeatedWorkShadowInput,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let dir = state_dir.join("metrics");
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path_with_timeout(
            &dir,
            StdDuration::ZERO,
        )?;
        fs::create_dir_all(&dir)?;
        let conn = Connection::open(dir.join("metrics.db"))?;
        conn.busy_timeout(StdDuration::ZERO)?;
        if let Err(error) = migrate_repeated_work_shadow(&conn) {
            if sqlite_is_busy(&error) {
                return Ok(false);
            }
            return Err(error.into());
        }
        if let Err(error) = Self::insert_repeated_work_shadow(&conn, input) {
            if sqlite_is_busy(&error) {
                return Ok(false);
            }
            return Err(error.into());
        }
        Ok(true)
    }

    /// Insert one observation on an already-fenced connection.
    fn insert_repeated_work_shadow(
        conn: &Connection,
        input: &RepeatedWorkShadowInput,
    ) -> Result<(), rusqlite::Error> {
        conn.execute(
            "INSERT INTO repeated_work_shadow
             (observed_at, kind, branch, sha, target_config, mode, job_id,
              superseded_job_id, superseded_sha)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                Utc::now().to_rfc3339(),
                input.kind.as_str(),
                input.branch,
                input.sha,
                input.target_config,
                input.mode,
                input.job_id,
                input.superseded_job_id,
                input.superseded_sha,
            ],
        )?;
        Ok(())
    }

    /// Return recent shadow repeated-work observations, newest first.
    pub fn repeated_work_shadow(
        &self,
        limit: usize,
    ) -> Result<Vec<RepeatedWorkShadowRow>, Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT observed_at, kind, branch, sha, target_config, mode, job_id,
                    superseded_job_id, superseded_sha
             FROM repeated_work_shadow
             ORDER BY observed_at DESC, id DESC
             LIMIT ?1",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = statement.query_map(params![limit], |row| {
            Ok(RepeatedWorkShadowRow {
                observed_at: row.get(0)?,
                kind: row.get(1)?,
                branch: row.get(2)?,
                sha: row.get(3)?,
                target_config: row.get(4)?,
                mode: row.get(5)?,
                job_id: row.get(6)?,
                superseded_job_id: row.get(7)?,
                superseded_sha: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Record one low-friction timing row.
    pub fn record(&self, input: &MetricRecordInput) -> Result<i64, Box<dyn std::error::Error>> {
        self.record_with_refresh(input, false)
    }

    /// Record a terminal external observation, refreshing a prior partial row
    /// with the same provider identity when necessary.
    #[allow(dead_code)]
    pub(crate) fn record_terminal_observation(
        &self,
        input: &MetricRecordInput,
    ) -> Result<i64, Box<dyn std::error::Error>> {
        if input.completed_at.is_none() {
            return Err("terminal metrics observation requires completed_at".into());
        }
        self.record_with_refresh(input, true)
    }

    pub(crate) fn record_terminal_observations(
        &self,
        inputs: &[MetricRecordInput],
    ) -> Result<usize, Box<dyn std::error::Error>> {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for input in inputs {
            if input.completed_at.is_none() {
                return Err("terminal metrics observation requires completed_at".into());
            }
            Self::record_with_conn(&tx, input, true)?;
        }
        tx.commit()?;
        Ok(inputs.len())
    }

    #[allow(clippy::too_many_lines)]
    fn record_with_refresh(
        &self,
        input: &MetricRecordInput,
        refresh_existing: bool,
    ) -> Result<i64, Box<dyn std::error::Error>> {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        let conn = self.connect()?;
        Self::record_with_conn(&conn, input, refresh_existing)
    }

    #[allow(clippy::too_many_lines)]
    fn record_with_conn(
        conn: &Connection,
        input: &MetricRecordInput,
        refresh_existing: bool,
    ) -> Result<i64, Box<dyn std::error::Error>> {
        let (project, repo) =
            project_key::normalize_for_write(&input.project, input.repo.as_deref());
        let normalized = MetricRecordInput {
            project,
            repo,
            ..input.clone()
        };
        let input = &normalized;
        let correlation_id = correlation_id_for(input);
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
            && let Some(existing) =
                existing_job_id(conn, provider, input.external_id.as_deref())?.or(
                    existing_unknown_attempt_id(conn, provider, input.external_id.as_deref())?,
                )
        {
            if refresh_existing {
                ensure_refresh_identity_compatible(conn, existing, input)?;
                let machine_id = upsert_machine(
                    conn,
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
                    conn,
                    existing,
                    machine_id,
                    input,
                    started_at,
                    completed_at,
                    measured_total_ms,
                    queued_at.as_deref(),
                    queue_ms,
                    input.correlation_id.as_deref(),
                )?;
            }
            return Ok(existing);
        }
        let machine_id = upsert_machine(
            conn,
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
            conn,
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
                event: input.event.clone(),
                provider_run_id: input.provider_run_id,
                run_attempt: input.run_attempt,
                head_sha: input.head_sha.clone(),
                provenance: input.provenance.clone(),
                provenance_verified: input.provenance_verified,
            },
        )?;
        let job_id = insert_job(
            conn,
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
                correlation_id,
                runner_assigned: input.runner_assigned,
                labels_json: labels_json(input),
            },
        )?;
        let started_at_text = started_at.to_rfc3339();
        let completed_at_text = completed_at.to_rfc3339();
        insert_step(
            conn,
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
        let correlation_id = value_str(value, "correlation_id")
            .map(str::to_owned)
            .or_else(|| correlation_id_for_parts(Some(&provider), external_id.as_deref()));
        let ts = value_str(value, "completed_at")
            .or_else(|| value_str(value, "started_at"))
            .map_or_else(|| Utc::now().to_rfc3339(), str::to_owned);
        let (project, repo) = project_key::normalize_for_write(
            value_str(value, "project").unwrap_or("unknown"),
            value_str(value, "repo"),
        );
        let run_id = insert_run(
            &conn,
            &RunInsert {
                ts,
                project,
                repo,
                branch: value_str(value, "branch").map(str::to_owned),
                sha: value_str(value, "sha").map(str::to_owned),
                pr: value_i64(value, "pr"),
                workflow: value_str(value, "workflow").map(str::to_owned),
                profile: value_str(value, "profile").map(str::to_owned),
                routing_decision: value_str(value, "routing_decision").map(str::to_owned),
                status: status.clone(),
                event: value_str(value, "event").map(str::to_owned),
                provider_run_id: value_i64(value, "provider_run_id"),
                run_attempt: value_i64(value, "run_attempt"),
                head_sha: value_str(value, "head_sha").map(str::to_owned),
                provenance: value_str(value, "provenance").map(str::to_owned),
                provenance_verified: false,
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
                correlation_id,
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

    /// Return persisted phase rows, optionally scoped to one stable
    /// correlation id. A phase is a row in the steps table; no phase is
    /// inferred from the job's total duration.
    pub fn phase_rows(
        &self,
        correlation_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MetricPhaseRow>, Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            "SELECT jobs.correlation_id, steps.step, steps.status, steps.duration_ms,
                    steps.started_at, steps.completed_at
               FROM steps
               JOIN jobs ON jobs.id = steps.job_id
              WHERE (?1 IS NULL OR jobs.correlation_id = ?1)
              ORDER BY steps.id
              LIMIT ?2",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = statement.query_map(params![correlation_id, limit], |row| {
            Ok(MetricPhaseRow {
                correlation_id: row.get(0)?,
                phase: row.get(1)?,
                status: row.get(2)?,
                duration_ms: row.get(3)?,
                started_at: row.get(4)?,
                completed_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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
        let provenance = scorecard_provenance(&conn, project, &cutoff)?;
        let strata = scorecard_strata(&conn, project, &cutoff)?;
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
        let wasted_samples = samples
            .iter()
            .filter(|(status, duration, _, _, _)| {
                duration.is_some_and(|value| value >= 0)
                    && (proxy::is_failure(status) || proxy::is_cancelled(status))
            })
            .count();
        let wasted_compute_ms = samples
            .iter()
            .filter(|(status, _, _, _, _)| proxy::is_failure(status) || proxy::is_cancelled(status))
            .filter_map(|(_, duration, _, _, _)| *duration)
            .filter(|value| *value >= 0)
            .fold(0_i64, i64::saturating_add);
        let rework_samples = samples
            .iter()
            .filter(|(status, duration, _, _, _)| {
                duration.is_some_and(|value| value >= 0) && is_failure_status(status)
            })
            .count();
        let rework_compute_ms = samples
            .iter()
            .filter(|(status, _, _, _, _)| is_failure_status(status))
            .filter_map(|(_, duration, _, _, _)| *duration)
            .filter(|value| *value >= 0)
            .fold(0_i64, i64::saturating_add);
        let coordination_ms = samples
            .iter()
            .filter_map(|(_, _, queue, _, _)| *queue)
            .filter(|value| *value >= 0)
            .fold(0_i64, i64::saturating_add);
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
                value: None,
            }
        } else if identified_pull_request_samples == samples.len() {
            ScorecardCoverage {
                status: "available".to_owned(),
                reason: "every completed job sample includes pull-request identity".to_owned(),
                value: None,
            }
        } else {
            ScorecardCoverage {
                status: "partial".to_owned(),
                reason: format!(
                    "{identified_pull_request_samples} of {} completed job samples include pull-request identity",
                    samples.len()
                ),
                value: None,
            }
        };
        let cache = load_scorecard_cache_samples(&conn, project, &cutoff)?;
        let cache_hits = cache.iter().filter(|hit| **hit).count();
        let (
            safe_merge_count,
            joined_merge_count,
            wasted_merge_ms,
            wasted_merge_samples,
            wasted_duration_gaps,
        ) =
            self.safe_merge_summary(project, &cutoff)?;
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
            wasted_compute_minutes: millis_to_minutes(wasted_compute_ms),
            wasted_compute_samples: wasted_samples,
            coordination_minutes: millis_to_minutes(coordination_ms),
            coordination_samples: queues.len(),
            rework_compute_minutes: millis_to_minutes(rework_compute_ms),
            rework_compute_samples: rework_samples,
            coordination_rework_minutes: millis_to_minutes(
                rework_compute_ms.saturating_add(coordination_ms),
            ),
            safely_merged_changes: ScorecardCoverage {
                status: if joined_merge_count == 0 {
                    "unavailable"
                } else if joined_merge_count < safe_merge_count {
                    "partial"
                } else {
                    "available"
                }
                .to_owned(),
                reason: format!(
                    "{safe_merge_count} distinct safe receipt heads; {joined_merge_count} have matched completed job evidence"
                ),
                // No value under "unavailable": a count with no joined job
                // evidence is not a measured safely merged change.
                #[allow(clippy::cast_precision_loss)]
                value: (joined_merge_count > 0).then_some(safe_merge_count as f64),
            },
            wasted_compute_per_safely_merged_change: if joined_merge_count == 0 {
                unavailable_merge_coverage()
            } else {
                ScorecardCoverage {
                    reason: if wasted_duration_gaps > 0 {
                        format!(
                            "{:.6} lower-bound wasted compute minutes among joined changes from {} failed/cancelled job samples; {wasted_duration_gaps} job durations were unavailable",
                            wasted_merge_per_change(wasted_merge_ms, joined_merge_count),
                            wasted_merge_samples
                        )
                    } else {
                        format!(
                            "{:.6} wasted compute minutes per safely merged change among joined changes from {} failed/cancelled job samples",
                            wasted_merge_per_change(wasted_merge_ms, joined_merge_count),
                            wasted_merge_samples
                        )
                    },
                    // An observed valid duration is a lower bound among the
                    // joined changes when any job attached to the same PR has
                    // a missing/negative duration. Keep the value for the
                    // measured rows, but do not present it as a complete cost
                    // claim for the whole receipt population.
                    status: if wasted_duration_gaps > 0 {
                        "partial".to_owned()
                    } else if joined_merge_count < safe_merge_count {
                        "partial".to_owned()
                    } else {
                        "available".to_owned()
                    },
                    value: Some(wasted_merge_per_change(wasted_merge_ms, joined_merge_count)),
                }
            },
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
                value: None,
            },
            model_token_use: ScorecardCoverage {
                status: "unavailable".to_owned(),
                reason: "job metrics do not store model call or token counters".to_owned(),
                value: None,
            },
            basis: basis.as_str(),
            proxies,
            comparison,
            provenance,
            strata,
        })
    }
}

fn ensure_refresh_identity_compatible(
    conn: &Connection,
    job_id: i64,
    input: &MetricRecordInput,
) -> Result<(), Box<dyn std::error::Error>> {
    let stored: (Option<String>, Option<i64>, Option<i64>, Option<String>) = conn.query_row(
        "SELECT event, run_attempt, provider_run_id, head_sha FROM runs WHERE id = (SELECT run_id FROM jobs WHERE id = ?1)",
        [job_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    for (name, old, new) in [
        ("event", stored.0, input.event.clone()),
        (
            "run_attempt",
            stored.1.map(|v| v.to_string()),
            input.run_attempt.map(|v| v.to_string()),
        ),
        (
            "provider_run_id",
            stored.2.map(|v| v.to_string()),
            input.provider_run_id.map(|v| v.to_string()),
        ),
        ("head_sha", stored.3, input.head_sha.clone()),
    ] {
        if let (Some(old), Some(new)) = (old, new)
            && old != new
        {
            return Err(format!("existing job identity conflict in {name}").into());
        }
    }
    Ok(())
}

fn existing_unknown_attempt_id(
    conn: &Connection,
    provider: &str,
    external_id: Option<&str>,
) -> Result<Option<i64>, rusqlite::Error> {
    if !matches!(provider, "github" | "self-hosted" | "github-hosted") {
        return Ok(None);
    }
    let Some(value) = external_id else {
        return Ok(None);
    };
    let parts = value.split('/').collect::<Vec<_>>();
    if parts.len() != 3 || !parts[0].starts_with("github:") {
        return Ok(None);
    }
    let prefix = format!("{}/{}/", parts[0], parts[1]);
    conn.query_row(
        "SELECT id FROM jobs
           WHERE provider IN ('github', 'self-hosted', 'github-hosted')
             AND external_id LIKE ?1 || '%'
           ORDER BY id DESC LIMIT 1",
        params![prefix],
        |row| row.get(0),
    )
    .optional()
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
    proxy::is_failure(status)
}

fn load_scorecard_samples(
    conn: &Connection,
    project: &str,
    cutoff: &str,
) -> Result<Vec<ScorecardSample>, rusqlite::Error> {
    let mut statement = conn.prepare(&format!(
        "SELECT job.status, job.total_ms, job.queue_ms, run.repo, run.pr
           FROM jobs job JOIN runs run ON run.id = job.run_id
          WHERE {} AND job.completed_at IS NOT NULL
            AND julianday(job.completed_at) >= julianday(?4)
          ORDER BY job.id",
        project_key::sql_filter("run.project", "run.repo")
    ))?;
    let key = ProjectKey::parse(project);
    let (any, short, full) = project_key::sql_params(key.as_ref());
    statement
        .query_map(params![any, short, full, cutoff], |row| {
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

fn scorecard_provenance(
    conn: &Connection,
    project: &str,
    cutoff: &str,
) -> Result<ScorecardProvenance, rusqlite::Error> {
    let mut statement = conn.prepare(&format!(
        "SELECT run.provenance, run.provenance_verified FROM jobs job JOIN runs run ON run.id = job.run_id
          WHERE {} AND job.completed_at IS NOT NULL
            AND julianday(job.completed_at) >= julianday(?4)",
        project_key::sql_filter("run.project", "run.repo")
    ))?;
    let key = ProjectKey::parse(project);
    let (any, short, full) = project_key::sql_params(key.as_ref());
    let mut counts = ScorecardProvenance::default();
    for row in statement.query_map(params![any, short, full, cutoff], |row| {
        Ok((row.get::<_, Option<String>>(0)?, row.get::<_, bool>(1)?))
    })? {
        let (provenance, verified) = row?;
        match (provenance.as_deref(), verified) {
            (Some("production"), true) => counts.production += 1,
            (Some("synthetic"), true) => counts.synthetic += 1,
            _ => counts.unknown += 1,
        }
    }
    // Promotion baselines use authenticated production rows only. Synthetic
    // rows remain a separate control population and unknown rows remain
    // visible, but neither can increase the production denominator.
    counts.eligible_samples = counts.production;
    counts.status = if counts.production == 0 {
        "unavailable".to_owned()
    } else if counts.synthetic == 0 && counts.unknown == 0 {
        "production_authenticated".to_owned()
    } else {
        "partial".to_owned()
    };
    Ok(counts)
}

fn scorecard_strata(
    conn: &Connection,
    project: &str,
    cutoff: &str,
) -> Result<ScorecardStrata, rusqlite::Error> {
    let mut statement = conn.prepare(&format!(
        "SELECT run.event, run.run_attempt, run.pr, run.provider_run_id, run.head_sha
           FROM jobs job JOIN runs run ON run.id = job.run_id
          WHERE {} AND job.completed_at IS NOT NULL
            AND julianday(job.completed_at) >= julianday(?4)",
        project_key::sql_filter("run.project", "run.repo")
    ))?;
    let key = ProjectKey::parse(project);
    let (any, short, full) = project_key::sql_params(key.as_ref());
    let mut strata = ScorecardStrata::default();
    for row in statement.query_map(params![any, short, full, cutoff], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })? {
        let (event, attempt, pr, run_id, head_sha) = row?;
        match (event.as_deref(), attempt, pr, run_id, head_sha.as_deref()) {
            (Some("pull_request"), Some(1), Some(_), _, _) => strata.normal_pr += 1,
            (Some("pull_request"), Some(attempt), Some(_), _, _) if attempt > 1 => {
                strata.rerun += 1;
            }
            (Some("merge_group"), _, _, Some(_), Some(head)) if !head.is_empty() => {
                strata.merge_group += 1;
            }
            _ => strata.unclassified += 1,
        }
    }
    strata.status = if strata.normal_pr + strata.merge_group + strata.rerun == 0 {
        "unavailable".to_owned()
    } else if strata.unclassified == 0 {
        "observed".to_owned()
    } else {
        "partial".to_owned()
    };
    Ok(strata)
}

fn load_scorecard_cache_samples(
    conn: &Connection,
    project: &str,
    cutoff: &str,
) -> Result<Vec<bool>, rusqlite::Error> {
    let mut statement = conn.prepare(&format!(
        "SELECT step.cache_hit
           FROM steps step
           JOIN jobs job ON job.id = step.job_id
           JOIN runs run ON run.id = job.run_id
          WHERE {} AND job.completed_at IS NOT NULL
            AND julianday(job.completed_at) >= julianday(?4)
            AND step.cache_hit IS NOT NULL",
        project_key::sql_filter("run.project", "run.repo")
    ))?;
    let key = ProjectKey::parse(project);
    let (any, short, full) = project_key::sql_params(key.as_ref());
    statement
        .query_map(params![any, short, full, cutoff], |row| row.get(0))?
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
fn millis_to_minutes(milliseconds: i64) -> f64 {
    milliseconds as f64 / 60_000.0
}

#[allow(clippy::cast_precision_loss)]
fn wasted_merge_per_change(milliseconds: i64, changes: usize) -> f64 {
    milliseconds as f64 / 60_000.0 / changes as f64
}

#[allow(clippy::cast_precision_loss)]
/// Failed and cancelled compute across every head of `repo`'s pull request
/// `pr`: (milliseconds, job samples). A row with no or a negative duration
/// is not counted. Runs are matched by PR number, so merge-group runs, which
/// carry none, are not counted. The query is not limited to the scorecard's
/// window: a change counted in the window brings all of its PR's waste.
fn pull_request_waste(
    conn: &rusqlite::Connection,
    repo: &str,
    pr: i64,
) -> Result<(i64, usize, usize), rusqlite::Error> {
    let mut q = conn.prepare(
        "SELECT job.status, job.total_ms FROM jobs job JOIN runs run ON run.id = job.run_id \
         WHERE lower(trim(run.repo)) = ?1 AND run.pr = ?2 AND job.completed_at IS NOT NULL",
    )?;
    let mut wasted = 0_i64;
    let mut samples = 0_usize;
    let mut duration_gaps = 0_usize;
    for row in q.query_map(rusqlite::params![repo, pr], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
    })? {
        let (status, ms) = row?;
        let Some(ms) = ms.filter(|value| *value >= 0) else {
            duration_gaps += 1;
            continue;
        };
        if crate::metrics::proxy::is_failure(&status)
            || crate::metrics::proxy::is_cancelled(&status)
        {
            wasted = wasted.saturating_add(ms);
            samples += 1;
        }
    }
    Ok((wasted, samples, duration_gaps))
}

fn unavailable_merge_coverage() -> ScorecardCoverage {
    ScorecardCoverage {
        status: "unavailable".to_owned(),
        reason: "no durable merge receipt joins a PR head to a safely merged change".to_owned(),
        value: None,
    }
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
        value: None,
    }
}

fn migrate_repeated_work_shadow(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS repeated_work_shadow (
           id INTEGER PRIMARY KEY,
           observed_at TEXT NOT NULL,
           kind TEXT NOT NULL,
           branch TEXT NOT NULL,
           sha TEXT NOT NULL,
           target_config TEXT NOT NULL,
           mode TEXT NOT NULL,
           job_id TEXT NOT NULL,
           superseded_job_id TEXT NOT NULL,
           superseded_sha TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS repeated_work_shadow_observed_at
           ON repeated_work_shadow(observed_at DESC);",
    )
}

fn sqlite_is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(inner.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
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
    event: Option<String>,
    provider_run_id: Option<i64>,
    run_attempt: Option<i64>,
    head_sha: Option<String>,
    provenance: Option<String>,
    provenance_verified: bool,
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
    correlation_id: Option<String>,
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
        "INSERT INTO runs (ts, project, repo, branch, sha, pr, workflow, profile, routing_decision, status, event, provider_run_id, run_attempt, head_sha, provenance, provenance_verified)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
            run.status,
            run.event,
            run.provider_run_id,
            run.run_attempt,
            run.head_sha,
            run.provenance,
            run.provenance_verified,
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
          correlation_id, runner_assigned, labels_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
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
            job.correlation_id,
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    correlation_id: Option<&str>,
) -> Result<(), rusqlite::Error> {
    let started_at = started_at.to_rfc3339();
    let completed_at = completed_at.to_rfc3339();
    let step = input.step.as_deref().unwrap_or("total");
    conn.execute(
        "UPDATE jobs
            SET queued_at = COALESCE(?2, queued_at), started_at = ?3, completed_at = ?4, run_ms = ?5, total_ms = ?5,
                queue_ms = COALESCE(?6, queue_ms), status = ?7, exit_code = COALESCE(?8, exit_code),
                failure_class = COALESCE(?9, failure_class), machine_id = ?10,
                target = COALESCE(?11, target), platform = COALESCE(?12, platform),
                backend = COALESCE(?13, backend), provider = COALESCE(?14, provider),
                correlation_id = COALESCE(?15, correlation_id),
                runner_assigned = COALESCE(?16, runner_assigned),
                labels_json = COALESCE(?17, labels_json)
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
            correlation_id,
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
                routing_decision = COALESCE(?10, routing_decision), status = ?11,
                event = COALESCE(event, ?12), provider_run_id = COALESCE(provider_run_id, ?13),
                run_attempt = COALESCE(run_attempt, ?14), head_sha = COALESCE(head_sha, ?15),
                provenance = CASE WHEN ?17 AND ?16 IS NOT NULL THEN ?16 ELSE provenance END,
                provenance_verified = MAX(provenance_verified, (?17 AND ?16 IS NOT NULL))
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
            input.event,
            input.provider_run_id,
            input.run_attempt,
            input.head_sha,
            input.provenance,
            input.provenance_verified,
        ],
    )?;
    let updated_steps = conn.execute(
        "UPDATE steps
            SET step = ?2, started_at = ?3, completed_at = ?4,
                duration_ms = ?5, status = ?6
          WHERE job_id = ?1 AND step = ?2",
        params![
            job_id,
            step,
            started_at,
            completed_at,
            input.duration_ms,
            input.status,
        ],
    )?;
    let updated_steps = if updated_steps == 0 && step != "total" {
        conn.execute(
            "UPDATE steps
                SET step = ?2, started_at = ?3, completed_at = ?4,
                    duration_ms = ?5, status = ?6
              WHERE job_id = ?1 AND step = 'total'",
            params![
                job_id,
                step,
                started_at,
                completed_at,
                input.duration_ms,
                input.status,
            ],
        )?
    } else {
        updated_steps
    };
    if updated_steps == 0 {
        insert_step(
            conn,
            job_id,
            step,
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

fn correlation_id_for(input: &MetricRecordInput) -> Option<String> {
    nonempty(input.correlation_id.clone()).or_else(|| {
        correlation_id_for_parts(input.provider.as_deref(), input.external_id.as_deref())
    })
}

fn correlation_id_for_parts(provider: Option<&str>, external_id: Option<&str>) -> Option<String> {
    nonempty(
        provider
            .zip(external_id)
            .map(|(provider, external_id)| format!("{provider}:{external_id}")),
    )
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
            let status = value_str(value, "status").unwrap_or("unknown");
            let updated = conn.execute(
                "UPDATE steps
                    SET duration_ms = ?3, status = ?4, cache_hit = ?5
                  WHERE job_id = ?1 AND step = ?2",
                params![
                    job_id,
                    phase,
                    ms,
                    status,
                    value.get("cache_hit").and_then(Value::as_bool),
                ],
            )?;
            if updated == 0 {
                insert_step(
                    conn,
                    job_id,
                    phase,
                    None,
                    None,
                    ms,
                    status,
                    value.get("cache_hit").and_then(Value::as_bool),
                )?;
            }
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
    let mut stmt = conn.prepare(&format!(
        "SELECT runs.project, runs.repo, runs.workflow, jobs.job, jobs.target, jobs.backend,
                jobs.provider, machines.name, jobs.status, jobs.total_ms, jobs.completed_at,
                jobs.external_id, jobs.correlation_id
         FROM jobs
         JOIN runs ON runs.id = jobs.run_id
         LEFT JOIN machines ON machines.id = jobs.machine_id
         WHERE {}",
        project_key::sql_filter("runs.project", "runs.repo")
    ))?;
    let key = project.and_then(ProjectKey::parse);
    let (any, short, full) = project_key::sql_params(key.as_ref());
    let rows = stmt.query_map(params![any, short, full], |row| {
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
            correlation_id: row.get(12)?,
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

fn load_summary_inputs(
    conn: &Connection,
    project: Option<&str>,
) -> Result<Vec<SummaryInput>, Box<dyn std::error::Error>> {
    let mut stmt = conn.prepare(&format!(
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
         WHERE {}",
        project_key::sql_filter("runs.project", "runs.repo")
    ))?;
    let key = project.and_then(ProjectKey::parse);
    let (any, short, full) = project_key::sql_params(key.as_ref());
    let rows = stmt.query_map(params![any, short, full], |row| {
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
    pub head_sha: Option<String>,
}

/// Provider-owned workflow run identity returned by GitHub's runs endpoint.
/// Missing fields remain unknown; callers must not infer them from event names
/// or pull-request shape.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct GithubRunMetadata {
    pub id: Option<i64>,
    pub run_attempt: Option<i64>,
    pub event: Option<String>,
    pub head_sha: Option<String>,
    pub head_branch: Option<String>,
}

/// Import one GitHub Actions job object as returned by `/actions/runs/{id}/jobs`.
#[must_use]
pub fn github_job_to_record(
    repo: &str,
    workflow: Option<&str>,
    project: &str,
    pr: Option<i64>,
    job: &GitHubRunJob,
) -> MetricRecordInput {
    let metadata = GithubRunMetadata {
        id: job.run_id,
        run_attempt: job.run_attempt,
        ..GithubRunMetadata::default()
    };
    github_job_to_record_with_run(repo, workflow, project, pr, job, &metadata)
}

/// Convert a GitHub job while retaining the authoritative workflow-run
/// identity that the jobs endpoint alone does not carry.
pub fn github_job_to_record_with_run(
    repo: &str,
    workflow: Option<&str>,
    project: &str,
    pr: Option<i64>,
    job: &GitHubRunJob,
    run: &GithubRunMetadata,
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
        sha: run.head_sha.clone(),
        branch: run.head_branch.clone(),
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
            job.run_attempt
                .map_or_else(|| "unknown".to_owned(), |attempt| attempt.to_string())
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
        event: run.event.clone(),
        provider_run_id: run.id.or(job.run_id),
        run_attempt: job.run_attempt,
        head_sha: run.head_sha.clone(),
        // GitHub's event field is descriptive identity only. Organic or
        // synthetic provenance requires an authenticated producer label.
        provenance: None,
        ..MetricRecordInput::default()
    }
}

#[cfg(test)]
mod instrument_tests;

#[cfg(test)]
mod tests {
    use std::time::Instant;

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

    fn test_receipt(
        id: &str,
        repo: &str,
        project: &str,
        pr: i64,
        head: char,
    ) -> crate::metrics::merge_receipt::MergeReceipt {
        crate::metrics::merge_receipt::MergeReceipt {
            schema: crate::metrics::merge_receipt::SCHEMA.to_owned(),
            receipt_id: id.to_owned(),
            repo: repo.to_owned(),
            project: project.to_owned(),
            base_branch: "main".to_owned(),
            pr,
            head_sha: head.to_string().repeat(40),
            validated_head_sha: head.to_string().repeat(40),
            merge_commit_sha: "b".repeat(40),
            merged_at: Utc::now(),
            safe_merge: true,
            source: "shipyard".to_owned(),
            required_checks: vec!["macos".to_owned()],
            gate_receipt_sha256: "c".repeat(64),
            evidence: format!("sha256:{id}"),
        }
    }

    #[test]
    fn merge_receipt_import_is_atomic_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(temp.path()).unwrap();
        let first = test_receipt("atomic-1", "owner/repo", "repo", 1, 'a');
        let mut bad = test_receipt("atomic-2", "owner/repo", "repo", 2, 'd');
        bad.head_sha = "BAD".into();
        assert!(store.import_merge_receipts(&[first.clone(), bad]).is_err());
        let count: i64 = store
            .connect()
            .unwrap()
            .query_row("SELECT count(*) FROM merge_receipts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(
            store
                .import_merge_receipts(std::slice::from_ref(&first))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .import_merge_receipts(std::slice::from_ref(&first))
                .unwrap(),
            0
        );
        let mut changed = first;
        changed.evidence = "sha256:changed".into();
        assert!(store.import_merge_receipts(&[changed]).is_err());
    }

    #[test]
    fn merge_receipt_semantic_conflicts_cover_base_head_and_merge() {
        let temp = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(temp.path()).unwrap();
        let base = test_receipt("semantic-1", "owner/repo", "repo", 7, 'a');
        store
            .import_merge_receipts(std::slice::from_ref(&base))
            .unwrap();
        for (id, mutate) in [
            ("semantic-base", 0),
            ("semantic-head", 1),
            ("semantic-merge", 2),
        ] {
            let mut r = base.clone();
            r.receipt_id = id.into();
            if mutate == 0 {
                r.base_branch = "release".into();
            }
            if mutate == 1 {
                r.head_sha = "d".repeat(40);
                r.validated_head_sha = r.head_sha.clone();
            }
            if mutate == 2 {
                r.merge_commit_sha = "e".repeat(40);
            }
            assert!(store.import_merge_receipts(&[r]).is_err(), "{id}");
        }
    }

    #[test]
    fn safe_merge_summary_is_windowed_and_join_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(temp.path()).unwrap();
        insert_scorecard_sample(&store, "repo", 10, "failure", 1000, 0, false);
        let conn = store.connect().unwrap();
        conn.execute(
            "UPDATE runs SET repo='owner/repo', sha=?1 WHERE pr=10",
            [&"a".repeat(40)],
        )
        .unwrap();
        let mut inside = test_receipt("window-in", "OWNER/REPO", "repo", 10, 'a');
        inside.merged_at = Utc::now();
        let mut old = test_receipt("window-old", "owner/repo", "repo", 11, 'd');
        old.merged_at = Utc::now() - Duration::days(10);
        store.import_merge_receipts(&[inside, old]).unwrap();
        let (total, joined, wasted, samples, gaps) = store
            .safe_merge_summary(
                "repo",
                (Utc::now() - Duration::days(2)).to_rfc3339().as_str(),
            )
            .unwrap();
        assert_eq!((total, joined, wasted, samples, gaps), (1, 1, 1000, 1, 0));
        let mut wrong_pr = test_receipt("wrong-pr", "owner/repo", "repo", 99, 'a');
        store.import_merge_receipts(&[wrong_pr.clone()]).unwrap();
        assert_eq!(
            store
                .safe_merge_summary("repo", "1970-01-01T00:00:00Z")
                .unwrap()
                .1,
            1
        );
        wrong_pr.repo = "other/repo".into();
        wrong_pr.receipt_id = "wrong-repo".into();
        assert!(
            store.import_merge_receipts(&[wrong_pr]).is_err()
                || store
                    .safe_merge_summary("repo", "1970-01-01T00:00:00Z")
                    .unwrap()
                    .1
                    == 1
        );
    }

    #[test]
    fn duplicate_receipts_for_one_head_count_once() {
        let temp = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(temp.path()).unwrap();
        insert_scorecard_sample(&store, "repo", 30, "success", 1000, 0, false);
        store
            .connect()
            .unwrap()
            .execute(
                "UPDATE runs SET repo='owner/repo', sha=?1 WHERE pr=30",
                [&"a".repeat(40)],
            )
            .unwrap();
        let first = test_receipt("dup-1", "owner/repo", "repo", 30, 'a');
        let mut second = first.clone();
        second.receipt_id = "dup-2".into();
        assert_eq!(store.import_merge_receipts(&[first, second]).unwrap(), 2);
        let (total, joined, _, _, _) = store
            .safe_merge_summary("repo", "1970-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(
            (total, joined),
            (1, 1),
            "two receipt ids, one head, one change"
        );
        let scorecard = store.stewardship_scorecard("repo", 2).unwrap();
        assert_eq!(scorecard.safely_merged_changes.value, Some(1.0));
    }

    #[test]
    fn waste_per_change_counts_every_head_of_the_pull_request() {
        let temp = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(temp.path()).unwrap();
        // An earlier push failed for 5 s; the merged head passed in 1 s.
        insert_scorecard_sample(&store, "repo", 31, "failure", 5000, 0, false);
        insert_scorecard_sample(&store, "repo", 31, "success", 1000, 0, false);
        let conn = store.connect().unwrap();
        conn.execute("UPDATE runs SET repo='owner/repo' WHERE pr=31", [])
            .unwrap();
        conn.execute(
            "UPDATE runs SET sha=?1 WHERE pr=31 AND status='failure'",
            [&"e".repeat(40)],
        )
        .unwrap();
        conn.execute(
            "UPDATE runs SET sha=?1 WHERE pr=31 AND status='success'",
            [&"a".repeat(40)],
        )
        .unwrap();
        // Control: another pull request's failure never counts.
        insert_scorecard_sample(&store, "repo", 32, "failure", 9000, 0, false);
        conn.execute("UPDATE runs SET repo='owner/repo' WHERE pr=32", [])
            .unwrap();
        store
            .import_merge_receipts(&[test_receipt("pr-31", "owner/repo", "repo", 31, 'a')])
            .unwrap();
        let summary = store
            .safe_merge_summary("repo", "1970-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(summary, (1, 1, 5000, 1, 0));
    }

    #[test]
    fn earlier_invalid_duration_keeps_waste_per_change_as_a_lower_bound() {
        let temp = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(temp.path()).unwrap();
        insert_scorecard_sample(&store, "repo", 34, "failure", 5000, 0, false);
        insert_scorecard_sample(&store, "repo", 34, "success", 1000, 0, false);
        let conn = store.connect().unwrap();
        conn.execute("UPDATE runs SET repo='owner/repo' WHERE pr=34", [])
            .unwrap();
        conn.execute(
            "UPDATE runs SET sha=?1 WHERE pr=34 AND status='failure'",
            [&"e".repeat(40)],
        )
        .unwrap();
        conn.execute(
            "UPDATE runs SET sha=?1 WHERE pr=34 AND status='success'",
            [&"a".repeat(40)],
        )
        .unwrap();
        conn.execute(
            "UPDATE jobs SET total_ms = NULL WHERE job = 'job-34-5000'",
            [],
        )
        .unwrap();
        store
            .import_merge_receipts(&[test_receipt("pr-34", "owner/repo", "repo", 34, 'a')])
            .unwrap();

        let summary = store
            .safe_merge_summary("repo", "1970-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(summary, (1, 1, 0, 0, 1));
        let scorecard = store.stewardship_scorecard("repo", 2).unwrap();
        assert_eq!(
            scorecard.wasted_compute_per_safely_merged_change.status,
            "partial"
        );
        assert!(scorecard
            .wasted_compute_per_safely_merged_change
            .reason
            .contains("lower-bound"));
        assert_eq!(
            scorecard.wasted_compute_per_safely_merged_change.value,
            Some(0.0)
        );

        conn.execute(
            "UPDATE jobs SET total_ms = -1 WHERE job = 'job-34-5000'",
            [],
        )
        .unwrap();
        assert_eq!(
            store
                .safe_merge_summary("repo", "1970-01-01T00:00:00Z")
                .unwrap(),
            (1, 1, 0, 0, 1)
        );
    }

    #[test]
    fn a_negative_duration_on_an_earlier_push_adds_no_waste() {
        let temp = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(temp.path()).unwrap();
        // The earlier push's failed job has a negative duration; the merged
        // head passed. The change still joins, and the bad row adds nothing.
        insert_scorecard_sample(&store, "repo", 34, "failure", -1, 0, false);
        insert_scorecard_sample(&store, "repo", 34, "success", 1000, 0, false);
        let conn = store.connect().unwrap();
        conn.execute("UPDATE runs SET repo='owner/repo' WHERE pr=34", [])
            .unwrap();
        conn.execute(
            "UPDATE runs SET sha=?1 WHERE pr=34 AND status='failure'",
            [&"e".repeat(40)],
        )
        .unwrap();
        conn.execute(
            "UPDATE runs SET sha=?1 WHERE pr=34 AND status='success'",
            [&"a".repeat(40)],
        )
        .unwrap();
        store
            .import_merge_receipts(&[test_receipt("pr-34", "owner/repo", "repo", 34, 'a')])
            .unwrap();
        let summary = store
            .safe_merge_summary("repo", "1970-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(summary, (1, 1, 0, 0));
    }

    #[test]
    fn an_unjoined_safe_merge_count_carries_no_value() {
        let temp = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(temp.path()).unwrap();
        store
            .import_merge_receipts(&[test_receipt("unjoined", "owner/repo", "repo", 33, 'a')])
            .unwrap();
        let scorecard = store.stewardship_scorecard("repo", 2).unwrap();
        assert_eq!(scorecard.safely_merged_changes.status, "unavailable");
        assert_eq!(scorecard.safely_merged_changes.value, None);
        assert!(
            scorecard
                .safely_merged_changes
                .reason
                .contains("1 distinct safe receipt heads")
        );
    }

    #[test]
    fn safe_merge_summary_accepts_owner_and_short_project_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(temp.path()).unwrap();
        let receipt = test_receipt("alias-1", "owner/repo", "repo", 2, 'a');
        store.import_merge_receipts(&[receipt]).unwrap();
        assert_eq!(
            store
                .safe_merge_summary("repo", "1970-01-01T00:00:00Z")
                .unwrap()
                .0,
            1
        );
        assert_eq!(
            store
                .safe_merge_summary("owner/repo", "1970-01-01T00:00:00Z")
                .unwrap()
                .0,
            1
        );
    }

    #[test]
    fn safe_merge_summary_rejects_wrong_repo_pr_and_sha_joins() {
        let temp = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(temp.path()).unwrap();
        insert_scorecard_sample(&store, "repo", 20, "failure", 1000, 0, false);
        let conn = store.connect().unwrap();
        conn.execute(
            "UPDATE runs SET repo='owner/repo', sha=?1 WHERE pr=20",
            [&"a".repeat(40)],
        )
        .unwrap();
        let mut wrong_sha = test_receipt("isolation-sha", "owner/repo", "repo", 20, 'd');
        wrong_sha.merged_at = Utc::now();
        let wrong_pr = test_receipt("isolation-pr", "owner/repo", "repo", 21, 'a');
        let wrong_repo = test_receipt("isolation-repo", "other/repo", "repo", 20, 'a');
        store
            .import_merge_receipts(&[wrong_sha, wrong_pr, wrong_repo])
            .unwrap();
        let summary = store
            .safe_merge_summary("repo", "1970-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(summary.0, 3);
        assert_eq!(summary.1, 0);
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
                event: None,
                provider_run_id: None,
                run_attempt: None,
                head_sha: None,
                provenance: None,
                provenance_verified: false,
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
                correlation_id: None,
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
        assert_eq!(
            rows[0].correlation_id.as_deref(),
            Some("tart-macos:github:1/2/1")
        );
        assert_eq!(
            store
                .phase_rows(Some("tart-macos:github:1/2/1"), 10)
                .expect("phase rows")
                .iter()
                .map(|row| row.phase.as_str())
                .collect::<Vec<_>>(),
            vec!["boot_to_ssh", "runner_process"]
        );
    }

    #[test]
    fn explicit_correlation_id_survives_terminal_refresh_and_scopes_phases() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        let external_id = Some("github:9/4/1".to_owned());
        let correlation_id = Some("pulp-run-9-4".to_owned());
        store
            .record(&MetricRecordInput {
                project: "pulp".to_owned(),
                job: "Build".to_owned(),
                provider: Some("github-hosted".to_owned()),
                external_id: external_id.clone(),
                correlation_id: correlation_id.clone(),
                step: Some("queue".to_owned()),
                duration_ms: 12,
                status: "in_progress".to_owned(),
                ..MetricRecordInput::default()
            })
            .expect("initial observation");
        store
            .record_terminal_observation(&MetricRecordInput {
                project: "pulp".to_owned(),
                job: "Build".to_owned(),
                provider: Some("github-hosted".to_owned()),
                external_id,
                correlation_id: None,
                step: Some("run".to_owned()),
                duration_ms: 34,
                status: "success".to_owned(),
                completed_at: Some(Utc::now()),
                ..MetricRecordInput::default()
            })
            .expect("terminal observation");
        let rows = store.list(Some("pulp"), 10).expect("list");
        assert_eq!(rows[0].correlation_id, correlation_id);
        let phases = store
            .phase_rows(Some("pulp-run-9-4"), 10)
            .expect("phase rows");
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].phase, "queue");
        assert_eq!(phases[0].status, "in_progress");
        assert_eq!(phases[1].phase, "run");
        assert_eq!(phases[1].status, "success");
    }

    #[test]
    fn a_slug_project_is_stored_as_the_short_key_plus_repo() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        store
            .record(&MetricRecordInput {
                project: "Generous-Corp/pulp".to_owned(),
                job: "macos".to_owned(),
                duration_ms: 1,
                status: "success".to_owned(),
                ..MetricRecordInput::default()
            })
            .unwrap();
        let rows = store.list(None, 10).unwrap();
        assert_eq!(rows[0].project, "pulp");
        assert_eq!(rows[0].repo.as_deref(), Some("Generous-Corp/pulp"));
        assert_eq!(store.list(Some("pulp"), 10).unwrap().len(), 1);
        assert_eq!(store.list(Some("generous-corp/pulp"), 10).unwrap().len(), 1);
        assert!(
            store
                .list(Some("someone-else/pulp"), 10)
                .unwrap()
                .is_empty()
        );
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
        assert_eq!(scorecard.provenance.status, "unavailable");
    }

    #[test]
    fn scorecard_reports_authenticated_and_unknown_provenance_separately() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        for (id, provenance) in [
            ("production", Some("production")),
            ("synthetic", Some("synthetic")),
            ("unknown", None),
        ] {
            store
                .record_terminal_observation(&MetricRecordInput {
                    project: "shipyard".to_owned(),
                    job: id.to_owned(),
                    status: "success".to_owned(),
                    duration_ms: 100,
                    completed_at: Some(Utc::now()),
                    external_id: Some(format!("test:{id}")),
                    provenance: provenance.map(str::to_owned),
                    provenance_verified: false,
                    ..MetricRecordInput::default()
                })
                .unwrap();
        }
        let scorecard = store.stewardship_scorecard("shipyard", 1).unwrap();
        assert_eq!(scorecard.provenance.production, 0);
        assert_eq!(scorecard.provenance.synthetic, 0);
        assert_eq!(scorecard.provenance.unknown, 3);
        assert_eq!(scorecard.provenance.eligible_samples, 0);
        assert_eq!(scorecard.provenance.status, "unavailable");

        store
            .record_terminal_observation(&MetricRecordInput {
                project: "shipyard".to_owned(),
                job: "verified".to_owned(),
                status: "success".to_owned(),
                duration_ms: 100,
                completed_at: Some(Utc::now()),
                external_id: Some("test:verified".to_owned()),
                provenance: Some("production".to_owned()),
                provenance_verified: true,
                ..MetricRecordInput::default()
            })
            .unwrap();
        let verified = store.stewardship_scorecard("shipyard", 1).unwrap();
        assert_eq!(verified.provenance.production, 1);
        assert_eq!(verified.provenance.eligible_samples, 1);
    }

    #[test]
    fn scorecard_classifies_event_attempt_and_excludes_unknown_event() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let rows = [
            (
                "pr",
                Some("pull_request"),
                Some(1),
                Some(9),
                None,
                Some("h1"),
            ),
            (
                "rerun",
                Some("pull_request"),
                Some(2),
                Some(9),
                None,
                Some("h1"),
            ),
            (
                "merge",
                Some("merge_group"),
                Some(1),
                None,
                Some(77),
                Some("h2"),
            ),
            (
                "manual",
                Some("workflow_dispatch"),
                Some(1),
                Some(9),
                None,
                Some("h3"),
            ),
        ];
        for (id, event, attempt, pr, provider_run_id, head_sha) in rows {
            store
                .record_terminal_observation(&MetricRecordInput {
                    project: "shipyard".to_owned(),
                    job: id.to_owned(),
                    status: "success".to_owned(),
                    duration_ms: 100,
                    completed_at: Some(Utc::now()),
                    external_id: Some(format!("test:{id}")),
                    event: event.map(str::to_owned),
                    run_attempt: attempt,
                    pr,
                    provider_run_id,
                    head_sha: head_sha.map(str::to_owned),
                    ..MetricRecordInput::default()
                })
                .unwrap();
        }
        let scorecard = store.stewardship_scorecard("shipyard", 1).unwrap();
        assert_eq!(scorecard.strata.normal_pr, 1);
        assert_eq!(scorecard.strata.rerun, 1);
        assert_eq!(scorecard.strata.merge_group, 1);
        assert_eq!(scorecard.strata.unclassified, 1);
        assert_eq!(scorecard.strata.status, "partial");
    }

    #[allow(clippy::type_complexity)]
    #[test]
    fn duplicate_job_does_not_relabel_first_seen_run_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let mut input = MetricRecordInput {
            project: "shipyard".to_owned(),
            job: "macos".to_owned(),
            status: "success".to_owned(),
            duration_ms: 100,
            completed_at: Some(Utc::now()),
            provider: Some("github".to_owned()),
            external_id: Some("github:7/1/1".to_owned()),
            event: Some("pull_request".to_owned()),
            run_attempt: Some(1),
            pr: Some(9),
            provider_run_id: Some(7),
            head_sha: Some("first".to_owned()),
            ..MetricRecordInput::default()
        };
        store.record_terminal_observation(&input).unwrap();
        input.event = Some("merge_group".to_owned());
        input.run_attempt = Some(2);
        input.pr = None;
        input.head_sha = Some("second".to_owned());
        assert!(store.record_terminal_observation(&input).is_err());
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        let identity: (
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<i64>,
        ) = conn
            .query_row(
                "SELECT event, run_attempt, pr, head_sha, provider_run_id FROM runs",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            identity,
            (
                Some("pull_request".to_owned()),
                Some(1),
                Some(9),
                Some("first".to_owned()),
                Some(7)
            )
        );
        let count: i64 = conn
            .query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn unknown_attempt_identity_refreshes_without_duplicate_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let mut input = MetricRecordInput {
            project: "shipyard".to_owned(),
            job: "macos".to_owned(),
            status: "success".to_owned(),
            duration_ms: 100,
            completed_at: Some(Utc::now()),
            provider: Some("github-hosted".to_owned()),
            external_id: Some("github:7/1/unknown".to_owned()),
            event: Some("pull_request".to_owned()),
            ..MetricRecordInput::default()
        };
        store.record_terminal_observation(&input).unwrap();
        input.external_id = Some("github:7/1/2".to_owned());
        input.run_attempt = Some(2);
        assert!(store.record_terminal_observation(&input).is_ok());
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn unknown_attempt_alias_does_not_cross_provider_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let base = MetricRecordInput {
            project: "shipyard".to_owned(),
            job: "macos".to_owned(),
            status: "success".to_owned(),
            duration_ms: 100,
            completed_at: Some(Utc::now()),
            provider: Some("github-hosted".to_owned()),
            external_id: Some("github:7/1/unknown".to_owned()),
            event: Some("pull_request".to_owned()),
            ..MetricRecordInput::default()
        };
        store.record_terminal_observation(&base).unwrap();
        let mut other = base.clone();
        other.provider = Some("tart-macos".to_owned());
        other.external_id = Some("github:7/1/2".to_owned());
        store.record_terminal_observation(&other).unwrap();
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM jobs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn unverified_label_cannot_be_promoted_by_verified_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let mut input = MetricRecordInput {
            project: "shipyard".to_owned(),
            job: "macos".to_owned(),
            status: "success".to_owned(),
            duration_ms: 100,
            completed_at: Some(Utc::now()),
            external_id: Some("github:8/1/1".to_owned()),
            provider: Some("github".to_owned()),
            provenance: Some("production".to_owned()),
            ..MetricRecordInput::default()
        };
        store.record_terminal_observation(&input).unwrap();
        input.provenance = Some("synthetic".to_owned());
        input.provenance_verified = true;
        store.record_terminal_observation(&input).unwrap();
        input.external_id = Some("github:8/2/1".to_owned());
        input.provenance = Some("production".to_owned());
        input.provenance_verified = false;
        store.record_terminal_observation(&input).unwrap();
        input.provenance = None;
        input.provenance_verified = true;
        store.record_terminal_observation(&input).unwrap();
        let scorecard = store.stewardship_scorecard("shipyard", 1).unwrap();
        assert_eq!(scorecard.provenance.production, 0);
        assert_eq!(scorecard.provenance.synthetic, 1);
        assert_eq!(scorecard.provenance.unknown, 1);
        assert_eq!(scorecard.provenance.eligible_samples, 0);
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
        assert!((scorecard.wasted_compute_minutes - 2.0).abs() < f64::EPSILON);
        assert_eq!(scorecard.wasted_compute_samples, 1);
        assert!((scorecard.coordination_minutes - 0.1).abs() < f64::EPSILON);
        assert_eq!(scorecard.coordination_samples, 3);
        assert!((scorecard.rework_compute_minutes - 2.0).abs() < f64::EPSILON);
        assert_eq!(scorecard.rework_compute_samples, 1);
        assert!((scorecard.coordination_rework_minutes - 2.1).abs() < f64::EPSILON);
        assert_eq!(scorecard.safely_merged_changes.status, "unavailable");
        assert_eq!(
            scorecard.wasted_compute_per_safely_merged_change.status,
            "unavailable"
        );
        assert_eq!(scorecard.distinct_pull_requests, 2);
        assert!((scorecard.pull_requests_per_day - 1.0).abs() < f64::EPSILON);
        assert_eq!(scorecard.pull_request_throughput.status, "available");
        assert_eq!(scorecard.cache_samples, 3);
        assert_eq!(scorecard.cache_hits, 2);
        assert_eq!(scorecard.cache_hit_rate, Some(2.0 / 3.0));
    }

    #[test]
    fn stewardship_scorecard_uses_only_imported_safe_merge_receipts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        insert_scorecard_sample(&store, "shipyard", 41, "failure", 60_000, 1_000, false);
        let conn = store.connect().expect("metrics connection");
        conn.execute(
            "UPDATE runs SET repo = 'generous-corp/shipyard' WHERE pr = 41",
            [],
        )
        .expect("repo identity");
        conn.execute("UPDATE runs SET sha = ?1 WHERE pr = 41", [&"a".repeat(40)])
            .expect("head identity");
        let receipt = crate::metrics::merge_receipt::MergeReceipt {
            schema: crate::metrics::merge_receipt::SCHEMA.to_owned(),
            receipt_id: "merge-41".to_owned(),
            repo: "Generous-Corp/Shipyard".to_owned(),
            project: "shipyard".to_owned(),
            base_branch: "main".to_owned(),
            pr: 41,
            head_sha: "a".repeat(40),
            validated_head_sha: "a".repeat(40),
            merge_commit_sha: "b".repeat(40),
            merged_at: Utc::now(),
            safe_merge: true,
            source: "shipyard".to_owned(),
            required_checks: vec!["macos".to_owned()],
            gate_receipt_sha256: "c".repeat(64),
            evidence: "sha256:test".to_owned(),
        };
        let mut unmatched = receipt.clone();
        unmatched.receipt_id = "merge-42".to_owned();
        unmatched.pr = 42;
        unmatched.head_sha = "d".repeat(40);
        unmatched.validated_head_sha = unmatched.head_sha.clone();
        assert_eq!(
            store
                .import_merge_receipts(&[receipt, unmatched])
                .expect("import"),
            2
        );
        let scorecard = store
            .stewardship_scorecard("shipyard", 2)
            .expect("scorecard");
        assert_eq!(scorecard.safely_merged_changes.status, "partial");
        assert_eq!(scorecard.safely_merged_changes.value, Some(2.0));
        assert!(
            scorecard
                .wasted_compute_per_safely_merged_change
                .reason
                .contains("1.000000")
        );
    }

    #[test]
    fn merge_receipts_normalize_keys_and_reject_semantic_project_conflicts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        let receipt = crate::metrics::merge_receipt::MergeReceipt {
            schema: crate::metrics::merge_receipt::SCHEMA.to_owned(),
            receipt_id: "merge-canonical-1".to_owned(),
            repo: " Generous-Corp/Shipyard ".to_owned(),
            project: " SHIPYARD ".to_owned(),
            base_branch: "main".to_owned(),
            pr: 41,
            head_sha: "a".repeat(40),
            validated_head_sha: "a".repeat(40),
            merge_commit_sha: "b".repeat(40),
            merged_at: Utc::now(),
            safe_merge: true,
            source: "shipyard".to_owned(),
            required_checks: vec!["macos".to_owned()],
            gate_receipt_sha256: "c".repeat(64),
            evidence: "sha256:canonical".to_owned(),
        };
        assert_eq!(
            store
                .import_merge_receipts(std::slice::from_ref(&receipt))
                .unwrap(),
            1
        );
        let conn = store.connect().unwrap();
        let keys: (String, String) = conn
            .query_row("SELECT repo, project FROM merge_receipts", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(
            keys,
            ("generous-corp/shipyard".to_owned(), "shipyard".to_owned())
        );

        let mut conflicting = receipt;
        conflicting.receipt_id = "merge-canonical-2".to_owned();
        conflicting.project = "other-project".to_owned();
        assert!(store.import_merge_receipts(&[conflicting]).is_err());
    }

    #[test]
    fn safe_merge_summary_rejects_null_or_negative_durations() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        insert_scorecard_sample(&store, "shipyard", 41, "failure", 60_000, 1_000, false);
        let conn = store.connect().unwrap();
        conn.execute(
            "UPDATE runs SET repo = 'generous-corp/shipyard', sha = ?1 WHERE pr = 41",
            [&"a".repeat(40)],
        )
        .unwrap();
        conn.execute(
            "UPDATE jobs SET total_ms = NULL WHERE job = 'job-41-60000'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO jobs (run_id, job, completed_at, total_ms, status) SELECT run_id, 'job-41-valid', completed_at, 500, 'failure' FROM jobs WHERE job = 'job-41-60000'",
            [],
        ).unwrap();
        let receipt = crate::metrics::merge_receipt::MergeReceipt {
            schema: crate::metrics::merge_receipt::SCHEMA.to_owned(),
            receipt_id: "merge-duration-1".to_owned(),
            repo: "GENEROUS-CORP/SHIPYARD".to_owned(),
            project: "shipyard".to_owned(),
            base_branch: "main".to_owned(),
            pr: 41,
            head_sha: "a".repeat(40),
            validated_head_sha: "a".repeat(40),
            merge_commit_sha: "b".repeat(40),
            merged_at: Utc::now(),
            safe_merge: true,
            source: "shipyard".to_owned(),
            required_checks: vec!["macos".to_owned()],
            gate_receipt_sha256: "c".repeat(64),
            evidence: "sha256:duration".to_owned(),
        };
        store.import_merge_receipts(&[receipt]).unwrap();
        let summary = store
            .safe_merge_summary("shipyard", "1970-01-01T00:00:00Z")
            .unwrap();
        assert_eq!(summary.0, 1);
        assert_eq!(summary.1, 0);
        conn.execute(
            "UPDATE jobs SET total_ms = -1 WHERE job = 'job-41-60000'",
            [],
        )
        .unwrap();
        assert_eq!(
            store
                .safe_merge_summary("shipyard", "1970-01-01T00:00:00Z")
                .unwrap()
                .1,
            0
        );
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
        insert_scorecard_sample(&store, "shipyard", 46, "fail", 60_000, 1_000, false);

        let scorecard = store
            .stewardship_scorecard("shipyard", 2)
            .expect("scorecard");

        assert_eq!(scorecard.job_samples, 6);
        assert_eq!(scorecard.successful_jobs, 1);
        assert_eq!(scorecard.failed_jobs, 2);
        assert_eq!(scorecard.other_jobs, 3);
        assert_eq!(scorecard.failure_rate, Some(2.0 / 3.0));
        assert_eq!(scorecard.wasted_compute_samples, 3);
        assert_eq!(scorecard.rework_compute_samples, 2);
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
            head_sha: None,
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
            head_sha: None,
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

    #[test]
    fn best_effort_shadow_write_drops_busy_sqlite_without_waiting() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        let lock = Connection::open(store.path()).expect("lock connection");
        lock.execute_batch("BEGIN EXCLUSIVE")
            .expect("exclusive lock");
        let input = RepeatedWorkShadowInput {
            kind: RepeatedWorkKind::DuplicateExactHeadConfig,
            branch: "main".to_owned(),
            sha: "head".to_owned(),
            target_config: "{}".to_owned(),
            mode: "full".to_owned(),
            job_id: "new".to_owned(),
            superseded_job_id: "old".to_owned(),
            superseded_sha: "head".to_owned(),
        };

        let started = Instant::now();
        assert!(
            !MetricsStore::record_repeated_work_shadow_best_effort(temp.path(), &input,)
                .expect("busy shadow write should be classified as dropped")
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        lock.execute_batch("ROLLBACK").expect("release lock");
    }

    #[test]
    fn terminal_batch_rolls_back_when_later_identity_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let base = MetricRecordInput {
            project: "shipyard".to_owned(),
            job: "macos".to_owned(),
            provider: Some("github-hosted".to_owned()),
            external_id: Some("github:9/1/1".to_owned()),
            status: "success".to_owned(),
            duration_ms: 1,
            completed_at: Some(Utc::now()),
            event: Some("pull_request".to_owned()),
            run_attempt: Some(1),
            provider_run_id: Some(9),
            head_sha: Some("head".to_owned()),
            ..Default::default()
        };
        store.record_terminal_observation(&base).unwrap();
        let mut unknown = base.clone();
        unknown.external_id = Some("github:9/3/unknown".to_owned());
        store.record_terminal_observation(&unknown).unwrap();
        let mut fresh = base.clone();
        fresh.external_id = Some("github:9/2/1".to_owned());
        let mut conflict = base.clone();
        conflict.external_id = Some("github:9/3/2".to_owned());
        conflict.event = Some("merge_group".to_owned());
        assert!(
            store
                .record_terminal_observations(&[fresh, conflict])
                .is_err()
        );
        let conn = Connection::open(store.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        let event: String = conn
            .query_row(
                "SELECT event FROM runs WHERE provider_run_id = 9 ORDER BY id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(event, "pull_request");
    }

    #[test]
    fn terminal_batch_rolls_back_when_sql_fails_after_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let base = MetricRecordInput {
            project: "shipyard".to_owned(),
            job: "macos".to_owned(),
            provider: Some("github".to_owned()),
            external_id: Some("github:11/1/1".to_owned()),
            status: "success".to_owned(),
            duration_ms: 10,
            completed_at: Some(Utc::now()),
            event: Some("pull_request".to_owned()),
            run_attempt: Some(1),
            provider_run_id: Some(11),
            head_sha: Some("original-head".to_owned()),
            ..Default::default()
        };
        store.record_terminal_observation(&base).unwrap();

        // Force the second insert to fail after the first batch item has
        // already written its run/job/step rows. This exercises the actual
        // SQLite transaction boundary rather than only prevalidation errors.
        let conn = Connection::open(store.path()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_metrics_batch_after_first_write
             BEFORE INSERT ON jobs
             WHEN NEW.job = 'trigger-sql-failure'
             BEGIN
               SELECT RAISE(ABORT, 'injected metrics write failure');
             END;",
        )
        .unwrap();

        let mut first = base.clone();
        first.external_id = Some("github:11/2/1".to_owned());
        first.job = "linux".to_owned();
        first.head_sha = Some("new-head".to_owned());
        let mut second = base.clone();
        second.external_id = Some("github:11/3/1".to_owned());
        second.job = "trigger-sql-failure".to_owned();
        second.head_sha = Some("failed-head".to_owned());

        let error = store
            .record_terminal_observations(&[first, second])
            .expect_err("the trigger must abort the second write");
        assert!(
            error.to_string().contains("injected metrics write failure"),
            "unexpected injected failure: {error}"
        );

        let conn = Connection::open(store.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM jobs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1,
            "the first batch row must be rolled back"
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM runs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1,
            "the first batch run must be rolled back"
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM steps", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1,
            "the first batch step must be rolled back"
        );
        let (external_id, head_sha): (String, String) = conn
            .query_row(
                "SELECT jobs.external_id, runs.head_sha
                 FROM jobs JOIN runs ON runs.id = jobs.run_id
                 WHERE jobs.external_id = 'github:11/1/1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(external_id, "github:11/1/1");
        assert_eq!(head_sha, "original-head");
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM jobs WHERE external_id IN ('github:11/2/1', 'github:11/3/1')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "neither new row may survive the failed batch"
        );
    }
}
