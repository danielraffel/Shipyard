//! Proxy-basis findings over the metrics store: loading samples and turning
//! window comparisons into [`MetricsFinding`]s.
//!
//! Proxy lanes are keyed by job class (target) only. Wall-time lanes also
//! split by backend and host, but a job cancelled before any runner was
//! assigned has no host, so a host-keyed lane would file every starved job
//! under a separate `unknown` lane and hide the starvation it should show.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, params};

use super::proxy::{self, Basis, Comparison, ProxySample, ProxyValue, Verdict};
use super::{Denominator, GateClass, MetricsFinding, job_name};

fn parse_time(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

/// Every job row with a completion time, as proxy samples.
pub(super) fn load_samples(
    conn: &Connection,
    project: Option<&str>,
) -> Result<Vec<ProxySample>, rusqlite::Error> {
    let mut statement = conn.prepare(&format!(
        "SELECT COALESCE(jobs.target, jobs.job, 'unknown'), jobs.status,
                runs.repo, runs.pr, jobs.queued_at, jobs.started_at,
                jobs.completed_at, jobs.total_ms, jobs.runner_assigned,
                (SELECT CASE WHEN COUNT(steps.cache_hit) = 0 THEN NULL
                             ELSE MIN(steps.cache_hit) END
                   FROM steps WHERE steps.job_id = jobs.id),
                jobs.job
           FROM jobs JOIN runs ON runs.id = jobs.run_id
          WHERE {} AND jobs.completed_at IS NOT NULL
          ORDER BY jobs.id",
        super::project_key::sql_filter("runs.project", "runs.repo")
    ))?;
    let key = project.and_then(super::ProjectKey::parse);
    let (any, short, full) = super::project_key::sql_params(key.as_ref());
    statement
        .query_map(params![any, short, full], |row| {
            let repo: Option<String> = row.get(2)?;
            let pr: Option<i64> = row.get(3)?;
            Ok(ProxySample {
                lane: row.get(0)?,
                status: row.get(1)?,
                pr: pr.map(|pr| (repo.unwrap_or_default(), pr)),
                queued_at: parse_time(row.get::<_, Option<String>>(4)?.as_deref()),
                started_at: parse_time(row.get::<_, Option<String>>(5)?.as_deref()),
                completed_at: parse_time(row.get::<_, Option<String>>(6)?.as_deref()),
                total_ms: row.get(7)?,
                runner_assigned: row.get(8)?,
                cache_hit: row.get(9)?,
                job: row.get(10)?,
            })
        })?
        .collect()
}

fn in_window(sample: &ProxySample, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
    sample
        .completed_at
        .is_some_and(|completed| completed >= from && completed < to)
}

type LaneWindows<'a> = BTreeMap<&'a str, (Vec<&'a ProxySample>, Vec<&'a ProxySample>)>;

/// Group samples by lane into `[start, split)` and `[split, end)`.
fn split_by_lane(
    samples: &[ProxySample],
    start: DateTime<Utc>,
    split: DateTime<Utc>,
    end: DateTime<Utc>,
) -> LaneWindows<'_> {
    let mut lanes: LaneWindows<'_> = BTreeMap::new();
    for sample in samples {
        if in_window(sample, start, split) {
            lanes.entry(&sample.lane).or_default().0.push(sample);
        } else if in_window(sample, split, end) {
            lanes.entry(&sample.lane).or_default().1.push(sample);
        }
    }
    lanes
}

fn severity(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Improved => "optimize",
        Verdict::Regressed => "investigate",
        Verdict::Mixed => "watch",
        Verdict::Unchanged | Verdict::InsufficientSample => "info",
    }
}

fn finding(
    lane: &str,
    signal: &str,
    comparison: Comparison,
    before: usize,
    after: usize,
) -> MetricsFinding {
    let verdict = comparison.verdict;
    let (signal, actions) = if verdict == Verdict::InsufficientSample {
        (
            "insufficient_samples",
            vec![
                "Keep collecting samples; each proxy names its minimum sample in `floor`."
                    .to_owned(),
            ],
        )
    } else {
        (
            signal,
            vec![
                "Act on the proxies; wall time is shown as load-dependent context only.".to_owned(),
                "Pass --basis wall-time to see the duration-based verdict.".to_owned(),
            ],
        )
    };
    MetricsFinding {
        severity: severity(verdict).to_owned(),
        lane: lane.to_owned(),
        signal: signal.to_owned(),
        message: comparison.summary(),
        sample_count: before + after,
        suggested_poll_interval_secs: if verdict == Verdict::Regressed {
            300
        } else {
            600
        },
        recommended_actions: actions,
        basis: comparison.basis.as_str(),
        comparison: Some(comparison),
        denominator: None,
        gate: None,
    }
}

/// Classify a lane from the job names it holds: `Required` when any of them
/// is a required status check, `Advisory` when none is, and `Unclassified`
/// when no required set is known.
pub(super) fn gate_class<'a>(
    job_names: impl IntoIterator<Item = &'a str>,
    required: &[String],
) -> GateClass {
    if required.is_empty() {
        return GateClass::Unclassified;
    }
    let is_required = job_names.into_iter().any(|name| {
        required
            .iter()
            .any(|check| job_name::matches(check, name) || job_name::canonical(name) == *check)
    });
    if is_required {
        GateClass::Required
    } else {
        GateClass::Advisory
    }
}

/// The denominator behind a lane's shares: job rows per window, each judged
/// by that job's own conclusion.
fn denominator(previous: &[&ProxySample], current: &[&ProxySample]) -> Denominator {
    let decided = |samples: &[&ProxySample]| {
        samples
            .iter()
            .filter(|sample| proxy::is_success(&sample.status) || proxy::is_failure(&sample.status))
            .count()
    };
    let mut job_names: Vec<String> = previous
        .iter()
        .chain(current)
        .map(|sample| sample.job.clone())
        .collect();
    job_names.sort();
    job_names.dedup();
    Denominator {
        unit: "jobs",
        previous_jobs: previous.len(),
        current_jobs: current.len(),
        previous_decided: decided(previous),
        current_decided: decided(current),
        job_names,
    }
}

/// `metrics compare` on the proxy basis: one finding per lane with samples on
/// both sides of the split.
pub(super) fn compare_findings(
    samples: &[ProxySample],
    split_days_ago: i64,
    now: DateTime<Utc>,
) -> Vec<MetricsFinding> {
    let split = now - Duration::days(split_days_ago.max(1));
    let lanes = split_by_lane(
        samples,
        DateTime::<Utc>::MIN_UTC,
        split,
        now + Duration::seconds(1),
    );
    lanes
        .into_iter()
        .filter(|(_, (before, after))| !before.is_empty() && !after.is_empty())
        .map(|(lane, (before, after))| {
            let comparison = proxy::compare(&before, &after, Basis::Proxy);
            finding(lane, "proxy_compare", comparison, before.len(), after.len())
        })
        .collect()
}

/// `metrics watch` on the proxy basis: the recent window against the one
/// before it, emitting only what an agent should look at.
pub(super) fn watch_findings(
    samples: &[ProxySample],
    since_days: i64,
    now: DateTime<Utc>,
    required: &[String],
) -> Vec<MetricsFinding> {
    let window = Duration::days(since_days.max(1));
    let current_start = now - window;
    let lanes = split_by_lane(
        samples,
        current_start - window,
        current_start,
        now + Duration::seconds(1),
    );
    let mut findings = Vec::new();
    for (lane, (previous, current)) in lanes {
        let comparison = proxy::compare(&previous, &current, Basis::Proxy);
        let lane_denominator = denominator(&previous, &current);
        let class = gate_class(
            lane_denominator.job_names.iter().map(String::as_str),
            required,
        );
        let before = findings.len();
        match comparison.verdict {
            Verdict::Regressed => findings.push(finding(
                lane,
                "proxy_regression",
                comparison,
                previous.len(),
                current.len(),
            )),
            Verdict::Mixed => findings.push(finding(
                lane,
                "proxy_mixed",
                comparison,
                previous.len(),
                current.len(),
            )),
            Verdict::InsufficientSample => findings.push(finding(
                lane,
                "insufficient_samples",
                comparison,
                previous.len(),
                current.len(),
            )),
            Verdict::Improved | Verdict::Unchanged
                if comparison.wall_time_verdict == Verdict::Regressed =>
            {
                let mut item = finding(
                    lane,
                    "load_dependent_slowdown",
                    comparison,
                    previous.len(),
                    current.len(),
                );
                "info".clone_into(&mut item.severity);
                item.recommended_actions = vec![
                    "Wall time rose but no proxy regressed: most likely host load, not the change."
                        .to_owned(),
                ];
                findings.push(item);
            }
            Verdict::Improved | Verdict::Unchanged => {}
        }
        for item in &mut findings[before..] {
            item.denominator = Some(lane_denominator.clone());
            item.gate = Some(class);
        }
    }
    // Required gates first: they are what blocks a merge.
    findings.sort_by_key(|item| match item.gate {
        Some(GateClass::Required) => 0,
        Some(GateClass::Unclassified) | None => 1,
        Some(GateClass::Advisory) => 2,
    });
    findings
}

/// `metrics trend`: every lane's earlier half vs later half of the window.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn trend_findings(
    samples: &[ProxySample],
    since_days: i64,
    now: DateTime<Utc>,
    basis: Basis,
) -> Vec<MetricsFinding> {
    let window = Duration::days(since_days.max(1));
    let start = now - window;
    let split = start + window / 2;
    split_by_lane(samples, start, split, now + Duration::seconds(1))
        .into_iter()
        .map(|(lane, (earlier, later))| {
            let comparison = proxy::compare(&earlier, &later, basis);
            finding(lane, "trend", comparison, earlier.len(), later.len())
        })
        .collect()
}

/// Project-wide proxies for `[now - window, now]` and their comparison with
/// the equal window before it. `None` when the window underflows time.
pub(super) fn scorecard_window(
    samples: &[ProxySample],
    now: DateTime<Utc>,
    window: Duration,
    basis: Basis,
) -> Option<(Vec<ProxyValue>, Comparison)> {
    let current_start = now.checked_sub_signed(window)?;
    let previous_start = current_start
        .checked_sub_signed(window)
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let end = now + Duration::seconds(1);
    let current: Vec<&ProxySample> = samples
        .iter()
        .filter(|sample| in_window(sample, current_start, end))
        .collect();
    let previous: Vec<&ProxySample> = samples
        .iter()
        .filter(|sample| in_window(sample, previous_start, current_start))
        .collect();
    Some((
        proxy::measure(&current),
        proxy::compare(&previous, &current, basis),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{MetricRecordInput, MetricsStore};
    use super::*;

    fn record(
        store: &MetricsStore,
        pr: i64,
        status: &str,
        queued: DateTime<Utc>,
        wait_s: i64,
        minutes: i64,
    ) {
        let started = queued + Duration::seconds(wait_s);
        store
            .record(&MetricRecordInput {
                project: "p".to_owned(),
                repo: Some("o/r".to_owned()),
                pr: Some(pr),
                job: "macos".to_owned(),
                target: Some("macos".to_owned()),
                backend: Some("local".to_owned()),
                host: Some("m3".to_owned()),
                duration_ms: minutes * 60_000,
                status: status.to_owned(),
                queued_at: Some(queued),
                started_at: Some(started),
                completed_at: Some(started + Duration::minutes(minutes)),
                runner_assigned: Some(true),
                ..MetricRecordInput::default()
            })
            .expect("record");
    }

    /// Ten days ago: quiet hosts, fast jobs, two attempts per PR and 30%
    /// failures. One day ago: a loaded host doubles every duration, but each
    /// PR lands in one clean attempt.
    fn load_confounded_store() -> (tempfile::TempDir, MetricsStore) {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        let old = Utc::now() - Duration::days(10);
        for index in 0..20 {
            let status = if index % 10 < 3 { "failure" } else { "success" };
            record(
                &store,
                index / 2,
                status,
                old + Duration::hours(index),
                60,
                10,
            );
        }
        let recent = Utc::now() - Duration::days(1);
        for index in 0..20 {
            record(
                &store,
                100 + index,
                "success",
                recent + Duration::seconds(index),
                100,
                20,
            );
        }
        (temp, store)
    }

    #[test]
    fn compare_verdict_follows_proxies_not_load() {
        let (_temp, store) = load_confounded_store();
        let findings = store.compare("p", 7, Basis::Proxy).expect("compare");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].lane, "macos");
        assert_eq!(findings[0].severity, "optimize");
        assert_eq!(findings[0].signal, "proxy_compare");
        assert_eq!(findings[0].basis, "proxy");
        let comparison = findings[0].comparison.as_ref().expect("comparison");
        assert_eq!(comparison.wall_time_verdict, Verdict::Regressed);
        assert!(findings[0].message.contains("context (load-dependent)"));

        let wall = store.compare("p", 7, Basis::WallTime).expect("compare");
        assert_eq!(wall.len(), 1, "{wall:?}");
        assert_eq!(wall[0].severity, "investigate");
        assert_eq!(wall[0].signal, "p50_total_ms_compare");
        assert_eq!(wall[0].basis, "wall_time");
    }

    #[test]
    fn watch_reports_a_load_only_slowdown_as_context_not_a_regression() {
        let (_temp, store) = load_confounded_store();
        let findings = store.watch("p", 7, Basis::Proxy).expect("watch");
        assert!(
            findings
                .iter()
                .all(|finding| finding.severity != "investigate"),
            "{findings:?}"
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.signal == "load_dependent_slowdown"),
            "{findings:?}"
        );
        let wall = store.watch("p", 7, Basis::WallTime).expect("watch");
        assert!(
            wall.iter()
                .any(|finding| finding.signal == "p90_total_ms_regression"),
            "{wall:?}"
        );
    }

    #[test]
    fn trend_and_scorecard_carry_the_proxy_verdict() {
        let (_temp, store) = load_confounded_store();
        let trend = store.trend(Some("p"), 16, Basis::Proxy).expect("trend");
        assert_eq!(trend.len(), 1, "{trend:?}");
        assert_eq!(
            trend[0].comparison.as_ref().map(|c| c.verdict),
            Some(Verdict::Improved)
        );
        let scorecard = store
            .stewardship_scorecard_with_basis("p", 7, Basis::Proxy)
            .expect("scorecard");
        assert_eq!(scorecard.basis, "proxy");
        assert_eq!(scorecard.comparison.verdict, Verdict::Improved);
        assert_eq!(scorecard.comparison.wall_time_verdict, Verdict::Regressed);
        let wall = store
            .stewardship_scorecard_with_basis("p", 7, Basis::WallTime)
            .expect("scorecard");
        assert_eq!(wall.comparison.verdict, Verdict::Regressed);
    }

    #[test]
    fn an_existing_store_gains_the_proxy_columns() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("metrics");
        std::fs::create_dir_all(&dir).expect("dir");
        let conn = Connection::open(dir.join("metrics.db")).expect("db");
        conn.execute_batch(
            "CREATE TABLE jobs (id INTEGER PRIMARY KEY, run_id INTEGER NOT NULL, job TEXT NOT NULL,
               status TEXT NOT NULL, provider TEXT, external_id TEXT, UNIQUE(provider, external_id));",
        )
        .expect("legacy table");
        drop(conn);
        let store = MetricsStore::open(temp.path()).expect("migrates");
        let conn = Connection::open(store.path()).expect("db");
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('jobs')")
            .expect("pragma")
            .query_map([], |row| row.get(0))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("names");
        assert!(columns.iter().any(|name| name == "runner_assigned"));
        assert!(columns.iter().any(|name| name == "labels_json"));
        // Reopening is idempotent.
        MetricsStore::open(temp.path()).expect("reopen");
    }
}
