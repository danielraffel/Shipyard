//! Bounded, fail-soft repository CI-health probes for the steward tick.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;

use super::GitHubActions;

const BILLING_MARKERS: [&str; 2] = ["recent account payments have failed", "spending limit"];
const QUEUED_LIMIT: Duration = Duration::hours(6);
const MAX_QUEUED_RUNS: u32 = 5;
/// The probe reads at most once per repository per this many minutes.
pub(super) const PROBE_INTERVAL_MINUTES: i64 = 30;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct CiHealthOverdue {
    pub kind: &'static str,
    pub condition: &'static str,
    pub repo: String,
    pub detail: String,
    pub at: DateTime<Utc>,
}

impl CiHealthOverdue {
    pub(super) fn key(&self) -> String {
        format!("{}:{}", self.repo.to_ascii_lowercase(), self.condition)
    }
}

pub(super) fn append_feed(
    state_dir: &std::path::Path,
    rows: &[CiHealthOverdue],
) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    let dir = state_dir.join("steward-dwell");
    std::fs::create_dir_all(&dir).map_err(|error| format!("ci health overdue feed: {error}"))?;
    let path = dir.join("overdue.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("ci health overdue feed {}: {error}", path.display()))?;
    use std::io::Write as _;
    for row in rows {
        let line = serde_json::to_string(row).map_err(|error| error.to_string())?;
        writeln!(file, "{line}").map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub(super) fn annotation_is_billing_blocked(annotation: &Value) -> bool {
    annotation
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|message| {
            let message = message.to_ascii_lowercase();
            BILLING_MARKERS
                .iter()
                .any(|marker| message.contains(marker))
        })
}

pub(super) fn annotations_show_billing_block(annotations: &Value) -> bool {
    annotations
        .as_array()
        .is_some_and(|items| items.iter().any(annotation_is_billing_blocked))
}

pub(super) fn queued_job_is_forever_queued(
    queued_at: &str,
    labels: &[String],
    online_labels: &std::collections::BTreeSet<String>,
    now: DateTime<Utc>,
) -> bool {
    let Ok(queued_at) = DateTime::parse_from_rfc3339(queued_at) else {
        return false;
    };
    if now.signed_duration_since(queued_at.with_timezone(&Utc)) <= QUEUED_LIMIT {
        return false;
    }
    // Only self-hosted jobs: a hosted job waits on GitHub, not on a label.
    if !labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case("self-hosted"))
    {
        return false;
    }
    labels.iter().any(|label| {
        !label.eq_ignore_ascii_case("self-hosted")
            && !online_labels.contains(&label.to_ascii_lowercase())
    })
}

fn online_runner_labels(raw: &Value) -> Option<std::collections::BTreeSet<String>> {
    let runners = raw.get("runners")?.as_array()?;
    let mut labels = std::collections::BTreeSet::new();
    for runner in runners {
        if runner.get("status").and_then(Value::as_str) != Some("online") {
            continue;
        }
        if let Some(items) = runner.pointer("/labels").and_then(Value::as_array) {
            labels.extend(items.iter().filter_map(|label| {
                label
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| name.to_ascii_lowercase())
            }));
        }
    }
    // An empty set is "unknown", never "nothing is served": org-level runners
    // (Generous-Corp) are not listed by the repository runners endpoint, and
    // ephemeral lanes have no runner online between jobs.
    if labels.is_empty() {
        return None;
    }
    Some(labels)
}

fn first_failed_job(raw: &Value) -> Option<(u64, Value)> {
    raw.get("jobs")?.as_array()?.iter().find_map(|job| {
        let conclusion = job.get("conclusion").and_then(Value::as_str)?;
        if conclusion != "failure" {
            return None;
        }
        Some((job.get("id")?.as_u64()?, job.clone()))
    })
}

/// Run both probes. Each failed read is returned as a loggable error and
/// produces no alert; a runners 403 is deliberately treated as unknown.
pub(super) fn probe(
    actions: &GitHubActions,
    repo: &str,
    default_branch: &str,
    now: DateTime<Utc>,
) -> (Vec<CiHealthOverdue>, Vec<String>) {
    let mut rows = Vec::new();
    let mut errors = Vec::new();

    match actions.list_runs_with_status(repo, "completed", Some(default_branch), 1) {
        Ok(runs) => {
            if let Some(run) = runs.first() {
                match actions.workflow_run_jobs_raw(repo, run.database_id) {
                    Ok(jobs) => {
                        if let Some((job_id, _)) = first_failed_job(&jobs) {
                            match actions.check_run_annotations_raw(repo, job_id) {
                                Ok(annotations) if annotations_show_billing_block(&annotations) => {
                                    rows.push(CiHealthOverdue {
                                        kind: "overdue",
                                        condition: "ci_billing_blocked",
                                        repo: repo.to_owned(),
                                        detail:
                                            "recent account payments have failed or spending limit"
                                                .to_owned(),
                                        at: now,
                                    });
                                }
                                Ok(_) => {}
                                Err(error) => errors.push(format!("ci_billing_blocked: {error:?}")),
                            }
                        }
                    }
                    Err(error) => errors.push(format!("ci_billing_blocked jobs: {error:?}")),
                }
            }
        }
        Err(error) => errors.push(format!("ci_billing_blocked runs: {error:?}")),
    }

    let online = match actions.repository_runners_raw(repo) {
        Ok(raw) => online_runner_labels(&raw),
        Err(error) => {
            errors.push(format!("ci_queued_forever runners unknown: {error:?}"));
            None
        }
    };
    if let Some(online_labels) = online {
        match actions.list_queued_runs(repo, MAX_QUEUED_RUNS) {
            Ok(runs) => {
                'runs: for run in runs {
                    let jobs = match actions.workflow_run_jobs_raw(repo, run.database_id) {
                        Ok(jobs) => jobs,
                        Err(error) => {
                            errors.push(format!("ci_queued_forever jobs: {error:?}"));
                            continue;
                        }
                    };
                    for job in jobs
                        .get("jobs")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if job.get("status").and_then(Value::as_str) != Some("queued") {
                            continue;
                        }
                        let labels = job
                            .get("labels")
                            .and_then(Value::as_array)
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        let queued_at = job
                            .get("queued_at")
                            .and_then(Value::as_str)
                            .unwrap_or(&run.created_at);
                        if queued_job_is_forever_queued(queued_at, &labels, &online_labels, now) {
                            rows.push(CiHealthOverdue {
                                kind: "overdue",
                                condition: "ci_queued_forever",
                                repo: repo.to_owned(),
                                detail: format!(
                                    "job {} labels={}",
                                    job.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                                    labels.join(",")
                                ),
                                at: now,
                            });
                            break 'runs;
                        }
                    }
                }
            }
            Err(error) => errors.push(format!("ci_queued_forever runs: {error:?}")),
        }
    }
    (rows, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_text_gives_one_match() {
        assert!(annotation_is_billing_blocked(
            &serde_json::json!({"message": "Recent account payments have failed"})
        ));
        assert!(!annotation_is_billing_blocked(
            &serde_json::json!({"message": "runner failed"})
        ));
    }

    #[test]
    fn queued_job_requires_six_hours_and_unserved_label() {
        let now = DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let served = ["linux".to_owned()].into_iter().collect();
        assert!(queued_job_is_forever_queued(
            "2026-11-18T05:00:00Z",
            &["self-hosted".into(), "retired".into()],
            &served,
            now
        ));
        assert!(!queued_job_is_forever_queued(
            "2026-11-18T07:00:00Z",
            &["self-hosted".into(), "retired".into()],
            &served,
            now
        ));
        assert!(!queued_job_is_forever_queued(
            "2026-11-18T05:00:00Z",
            &["self-hosted".into(), "linux".into()],
            &served,
            now
        ));
    }

    #[test]
    fn a_hosted_job_and_an_empty_runner_list_never_alert() {
        let online = std::collections::BTreeSet::from(["linux".to_owned()]);
        let old = "2026-11-18T00:00:00Z";
        let now = DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(!queued_job_is_forever_queued(
            old,
            &["ubuntu-latest".to_owned()],
            &online,
            now
        ));
        assert!(queued_job_is_forever_queued(
            old,
            &["self-hosted".to_owned(), "pulp-host-macpro".to_owned()],
            &online,
            now
        ));
        assert!(online_runner_labels(&serde_json::json!({"runners": []})).is_none());
    }
}
