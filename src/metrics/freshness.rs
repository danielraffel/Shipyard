//! How old is the data a metrics verdict rests on?
//!
//! Nothing in the store updates itself: GitHub rows arrive only when
//! `metrics import github` runs (by hand, or from the daemon's
//! `[metrics.import]` job). When that stops, every window a verdict looks at
//! slowly empties and `watch`/`advise` report "insufficient samples" — which
//! blames the lanes for a missing import. Freshness names the real cause.
//!
//! Samples are attributed to a *source*:
//!
//! * `github` — rows written by `metrics import github` (a `github_job` step);
//! * `recorded` — everything else (`metrics record`, `run command`, tartci,
//!   governed builds).
//!
//! The verdict is `stale` when any imported source (`github`) is older than
//! the threshold — a fresh locally recorded row must not hide a dead import —
//! or, for a project with no imported source, when its newest row is. A
//! project with no rows at all is `empty`, which is also what a wrong
//! `--project` key looks like.

use chrono::{DateTime, Duration, Utc};
use rusqlite::params;
use serde::Serialize;

use super::MetricsStore;
use super::project_key::{self, ProjectKey};

/// Default age past which a store is stale.
pub const DEFAULT_STALE_AFTER_HOURS: i64 = 24;

/// Source label for rows written by `metrics import github`.
pub const SOURCE_GITHUB: &str = "github";
/// Source label for every other row.
pub const SOURCE_RECORDED: &str = "recorded";

/// Overall freshness verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessStatus {
    /// Every judged source is within the threshold.
    Fresh,
    /// At least one judged source is older than the threshold.
    Stale,
    /// No rows match the project.
    Empty,
}

impl FreshnessStatus {
    /// Stable lower-case label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Empty => "empty",
        }
    }
}

/// Newest sample from one source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SourceFreshness {
    pub source: String,
    pub samples: usize,
    pub newest_sample_at: DateTime<Utc>,
    pub age_secs: i64,
    pub stale: bool,
    /// Whether this source decides the verdict (imported sources always do;
    /// `recorded` only when no imported source exists).
    pub judged: bool,
}

/// Freshness of the rows behind a verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Freshness {
    pub status: FreshnessStatus,
    /// The project argument, as given; `None` for the whole store.
    pub project: Option<String>,
    pub threshold_secs: i64,
    pub checked_at: DateTime<Utc>,
    /// Newest sample from any source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_sample_at: Option<DateTime<Utc>>,
    pub sources: Vec<SourceFreshness>,
    /// Sources that made the verdict `stale`.
    pub stale_sources: Vec<String>,
    /// One-line human summary, prefixed `STALE:`/`EMPTY:`/`fresh:`.
    pub message: String,
}

impl Freshness {
    /// Whether the verdict should be surfaced as a warning.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.status != FreshnessStatus::Fresh
    }
}

/// One stored row reduced to what freshness needs.
#[derive(Clone, Debug)]
pub struct FreshnessRow {
    pub source: String,
    pub at: DateTime<Utc>,
}

/// Parse `24h`, `90m`, `2d`, `3600s` (or a bare number of hours).
///
/// # Errors
/// When the value is not a positive duration.
pub fn parse_stale_after(text: &str) -> Result<Duration, String> {
    let trimmed = text.trim();
    let (number, unit) = match trimmed.char_indices().last() {
        Some((index, unit)) if unit.is_ascii_alphabetic() => (&trimmed[..index], unit),
        _ => (trimmed, 'h'),
    };
    let value: i64 = number
        .trim()
        .parse()
        .map_err(|_| format!("invalid staleness threshold {text:?}; use e.g. 24h, 90m, 2d"))?;
    if value <= 0 {
        return Err(format!("staleness threshold {text:?} must be positive"));
    }
    match unit.to_ascii_lowercase() {
        's' => Ok(Duration::seconds(value)),
        'm' => Ok(Duration::minutes(value)),
        'h' => Ok(Duration::hours(value)),
        'd' => Ok(Duration::days(value)),
        _ => Err(format!(
            "invalid staleness threshold unit in {text:?}; use s, m, h or d"
        )),
    }
}

/// Compact age text: `3d 4h`, `5h 12m`, `42m`, `30s`.
#[must_use]
pub fn age_text(age: Duration) -> String {
    let secs = age.num_seconds().max(0);
    let (days, hours, minutes) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if days > 0 && hours > 0 {
        format!("{days}d {hours}h")
    } else if days > 0 {
        format!("{days}d")
    } else if hours > 0 && minutes > 0 {
        format!("{hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
    }
}

/// Judge freshness from rows already filtered to one project.
#[must_use]
pub fn evaluate(
    project: Option<&str>,
    rows: &[FreshnessRow],
    threshold: Duration,
    now: DateTime<Utc>,
) -> Freshness {
    let mut by_source: std::collections::BTreeMap<&str, (usize, DateTime<Utc>)> =
        std::collections::BTreeMap::new();
    for row in rows {
        let entry = by_source.entry(row.source.as_str()).or_insert((0, row.at));
        entry.0 += 1;
        if row.at > entry.1 {
            entry.1 = row.at;
        }
    }
    let has_import = by_source.keys().any(|source| *source != SOURCE_RECORDED);
    let sources: Vec<SourceFreshness> = by_source
        .into_iter()
        .map(|(source, (samples, newest))| {
            let age = now - newest;
            SourceFreshness {
                source: source.to_owned(),
                samples,
                newest_sample_at: newest,
                age_secs: age.num_seconds(),
                stale: age > threshold,
                judged: !has_import || source != SOURCE_RECORDED,
            }
        })
        .collect();
    let newest_sample_at = sources.iter().map(|source| source.newest_sample_at).max();
    let stale: Vec<&SourceFreshness> = sources
        .iter()
        .filter(|source| source.judged && source.stale)
        .collect();
    let scope = project.map_or_else(|| "metrics store".to_owned(), |p| format!("project {p}"));
    let threshold_text = age_text(threshold);
    let (status, message) = if sources.is_empty() {
        (
            FreshnessStatus::Empty,
            format!(
                "EMPTY: no metrics rows for {scope}; run `shipyard metrics import github --repo \
                 <owner/repo>` or enable [metrics.import]"
            ),
        )
    } else if let Some(oldest) = stale.iter().min_by_key(|source| source.newest_sample_at) {
        let what = if oldest.source == SOURCE_RECORDED {
            "sample".to_owned()
        } else {
            format!("{} import", oldest.source)
        };
        (
            FreshnessStatus::Stale,
            format!(
                "STALE: last {what} {} ({} ago; threshold {threshold_text}) for {scope}; \
                 verdicts below rest on old data — run `shipyard metrics import github` or \
                 enable [metrics.import]",
                oldest.newest_sample_at.to_rfc3339(),
                age_text(now - oldest.newest_sample_at),
            ),
        )
    } else {
        let newest = newest_sample_at.unwrap_or(now);
        (
            FreshnessStatus::Fresh,
            format!(
                "fresh: newest sample {} ({} ago; threshold {threshold_text}) for {scope}",
                newest.to_rfc3339(),
                age_text(now - newest),
            ),
        )
    };
    Freshness {
        status,
        project: project.map(str::to_owned),
        threshold_secs: threshold.num_seconds(),
        checked_at: now,
        newest_sample_at,
        stale_sources: stale.iter().map(|source| source.source.clone()).collect(),
        sources,
        message,
    }
}

impl MetricsStore {
    /// Freshness of the rows a `project` lookup (either key form) reaches.
    ///
    /// # Errors
    /// On a database error.
    pub fn freshness(
        &self,
        project: Option<&str>,
        threshold: Duration,
        now: DateTime<Utc>,
    ) -> Result<Freshness, Box<dyn std::error::Error>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(&format!(
            "SELECT CASE WHEN EXISTS (SELECT 1 FROM steps
                                       WHERE steps.job_id = jobs.id
                                         AND steps.step = 'github_job')
                         THEN '{SOURCE_GITHUB}' ELSE '{SOURCE_RECORDED}' END,
                    COALESCE(jobs.completed_at, runs.ts)
               FROM jobs JOIN runs ON runs.id = jobs.run_id
              WHERE {}",
            project_key::sql_filter("runs.project", "runs.repo")
        ))?;
        let key = project.and_then(ProjectKey::parse);
        let (any, short, full) = project_key::sql_params(key.as_ref());
        let rows: Vec<FreshnessRow> = statement
            .query_map(params![any, short, full], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .filter_map(Result::ok)
            .filter_map(|(source, at)| {
                let at = DateTime::parse_from_rfc3339(at.as_deref()?).ok()?;
                Some(FreshnessRow {
                    source,
                    at: at.with_timezone(&Utc),
                })
            })
            .collect();
        Ok(evaluate(project, &rows, threshold, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricRecordInput, MetricsStore};

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn row(source: &str, when: &str) -> FreshnessRow {
        FreshnessRow {
            source: source.to_owned(),
            at: at(when),
        }
    }

    #[test]
    fn a_recent_import_is_fresh() {
        let now = at("2026-09-30T12:00:00Z");
        let fresh = evaluate(
            Some("pulp"),
            &[row(SOURCE_GITHUB, "2026-09-30T10:00:00Z")],
            Duration::hours(24),
            now,
        );
        assert_eq!(fresh.status, FreshnessStatus::Fresh);
        assert!(fresh.message.starts_with("fresh:"), "{}", fresh.message);
    }

    #[test]
    fn an_old_import_is_stale_even_beside_fresh_recorded_rows() {
        let now = at("2026-09-30T12:00:00Z");
        let freshness = evaluate(
            Some("pulp"),
            &[
                row(SOURCE_GITHUB, "2026-09-27T06:33:45Z"),
                row(SOURCE_RECORDED, "2026-09-30T11:59:00Z"),
            ],
            Duration::hours(24),
            now,
        );
        assert_eq!(freshness.status, FreshnessStatus::Stale);
        assert_eq!(freshness.stale_sources, vec![SOURCE_GITHUB.to_owned()]);
        assert!(
            freshness
                .message
                .starts_with("STALE: last github import 2026-09-27T06:33:45+00:00 (3d 5h ago"),
            "{}",
            freshness.message
        );
    }

    #[test]
    fn recorded_rows_decide_only_without_an_import_source() {
        let now = at("2026-09-30T12:00:00Z");
        let old = evaluate(
            None,
            &[row(SOURCE_RECORDED, "2026-09-20T00:00:00Z")],
            Duration::hours(24),
            now,
        );
        assert_eq!(old.status, FreshnessStatus::Stale);
        let judged_import = evaluate(
            None,
            &[
                row(SOURCE_RECORDED, "2026-09-20T00:00:00Z"),
                row(SOURCE_GITHUB, "2026-09-30T11:00:00Z"),
            ],
            Duration::hours(24),
            now,
        );
        assert_eq!(judged_import.status, FreshnessStatus::Fresh);
    }

    #[test]
    fn no_rows_is_empty_not_insufficient() {
        let freshness = evaluate(Some("nope"), &[], Duration::hours(24), Utc::now());
        assert_eq!(freshness.status, FreshnessStatus::Empty);
        assert!(freshness.message.starts_with("EMPTY:"));
    }

    #[test]
    fn ages_read_compactly() {
        assert_eq!(age_text(Duration::hours(24)), "1d");
        assert_eq!(age_text(Duration::hours(77)), "3d 5h");
        assert_eq!(age_text(Duration::minutes(90)), "1h 30m");
        assert_eq!(age_text(Duration::hours(2)), "2h");
        assert_eq!(age_text(Duration::seconds(42)), "42s");
    }

    #[test]
    fn thresholds_parse_units() {
        assert_eq!(parse_stale_after("24h").unwrap(), Duration::hours(24));
        assert_eq!(parse_stale_after("90m").unwrap(), Duration::minutes(90));
        assert_eq!(parse_stale_after("2d").unwrap(), Duration::days(2));
        assert_eq!(parse_stale_after("6").unwrap(), Duration::hours(6));
        assert!(parse_stale_after("0h").is_err());
        assert!(parse_stale_after("soon").is_err());
        assert!(parse_stale_after("3w").is_err());
    }

    fn github_row(project: &str, repo: &str, completed: &str, id: i64) -> MetricRecordInput {
        MetricRecordInput {
            project: project.to_owned(),
            repo: Some(repo.to_owned()),
            job: "macos".to_owned(),
            provider: Some("self-hosted".to_owned()),
            step: Some("github_job".to_owned()),
            duration_ms: 1_000,
            status: "success".to_owned(),
            external_id: Some(format!("github:1/{id}/1")),
            completed_at: Some(at(completed)),
            ..MetricRecordInput::default()
        }
    }

    #[test]
    fn store_freshness_is_per_project_and_reachable_by_either_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        store
            .record(&github_row(
                "pulp",
                "Generous-Corp/pulp",
                "2026-09-27T06:33:45Z",
                1,
            ))
            .unwrap();
        store
            .record(&github_row(
                "shipyard",
                "danielraffel/Shipyard",
                "2026-09-30T11:00:00Z",
                2,
            ))
            .unwrap();
        let now = at("2026-09-30T12:00:00Z");
        for key in ["pulp", "Generous-Corp/pulp", "generous-corp/PULP"] {
            let freshness = store
                .freshness(Some(key), Duration::hours(24), now)
                .unwrap();
            assert_eq!(freshness.status, FreshnessStatus::Stale, "{key}");
            assert_eq!(freshness.sources[0].samples, 1, "{key}");
        }
        let shipyard = store
            .freshness(Some("danielraffel/Shipyard"), Duration::hours(24), now)
            .unwrap();
        assert_eq!(shipyard.status, FreshnessStatus::Fresh);
        let missing = store
            .freshness(Some("someone-else/pulp"), Duration::hours(24), now)
            .unwrap();
        assert_eq!(missing.status, FreshnessStatus::Empty);
    }
}
