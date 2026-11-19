//! Bounded, fail-soft repository CI-health probes for the steward tick.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::Write;

use super::GitHubActions;

const BILLING_MARKERS: [&str; 2] = ["recent account payments have failed", "spending limit"];
const QUEUED_LIMIT: Duration = Duration::hours(6);
const MAX_QUEUED_RUNS: u32 = 5;
const MAX_BILLING_RUNS: u32 = 5;
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

#[cfg(test)]
pub(super) fn queued_job_is_forever_queued(
    queued_at: &str,
    labels: &[String],
    online_labels: &std::collections::BTreeSet<String>,
    now: DateTime<Utc>,
) -> bool {
    let runner_sets = if online_labels.is_empty() {
        &[][..]
    } else {
        std::slice::from_ref(online_labels)
    };
    queued_job_is_forever_queued_on_runners(queued_at, labels, runner_sets, now)
}

fn queued_job_is_forever_queued_on_runners(
    queued_at: &str,
    labels: &[String],
    online_runners: &[BTreeSet<String>],
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
    let constrained_labels = labels
        .iter()
        .filter(|label| !label.eq_ignore_ascii_case("self-hosted"));
    if labels
        .iter()
        .all(|label| label.eq_ignore_ascii_case("self-hosted"))
    {
        // A bare `runs-on: self-hosted` job can use any online runner. It is
        // overdue only when the successfully read inventory has no eligible
        // runner at all.
        return online_runners.is_empty();
    }
    online_runners.iter().all(|runner| {
        !constrained_labels
            .clone()
            .all(|label| runner.contains(&label.to_ascii_lowercase()))
    })
}

fn online_runner_label_sets(raw: &Value) -> Option<Vec<BTreeSet<String>>> {
    let runners = raw.get("runners")?.as_array()?;
    if runners.is_empty() {
        return None;
    }
    let mut online = Vec::new();
    for runner in runners {
        let status = runner.get("status").and_then(Value::as_str)?;
        let is_online = match status {
            "online" => true,
            "offline" => false,
            _ => return None,
        };
        let labels = runner
            .get("labels")?
            .as_array()?
            .iter()
            .map(|label| label.get("name")?.as_str().map(str::to_ascii_lowercase))
            .collect::<Option<BTreeSet<_>>>()?;
        if is_online {
            online.push(labels);
        }
    }
    Some(online)
}

/// Commit condition state only after the overdue feed has been written.
/// Probe errors simply omit that condition from `successful_conditions`, so a
/// transient read failure cannot clear or suppress its prior state.
pub(super) fn commit_seen_after_feed<F>(
    seen: &mut BTreeSet<String>,
    repo: &str,
    rows: &[CiHealthOverdue],
    successful_conditions: &BTreeSet<&'static str>,
    append: F,
) -> Result<(), String>
where
    F: FnOnce(&[CiHealthOverdue]) -> Result<(), String>,
{
    let mut next_seen = seen.clone();
    let mut new_rows = Vec::new();
    let prefix = format!("{}:", repo.to_ascii_lowercase());
    for row in rows {
        if !seen.contains(&row.key()) {
            new_rows.push(row.clone());
        }
        next_seen.insert(row.key());
    }
    for condition in successful_conditions {
        next_seen.retain(|key| !key.ends_with(condition) || !key.starts_with(&prefix));
        for row in rows.iter().filter(|row| row.condition == *condition) {
            next_seen.insert(row.key());
        }
    }
    append(&new_rows)?;
    *seen = next_seen;
    Ok(())
}

fn online_labels_or_unknown(
    actions: &GitHubActions,
    repo: &str,
    diagnostics: &mut Vec<String>,
) -> Option<Vec<BTreeSet<String>>> {
    match actions.repository_runners_raw(repo) {
        Ok(raw) => {
            let owner = repo.split_once('/').map_or(repo, |(owner, _)| owner);
            match actions.organization_runners_raw(owner) {
                Ok(org_raw) => {
                    let Some(org_runners) = org_raw.get("runners").and_then(Value::as_array) else {
                        diagnostics.push(
                            "ci_queued_forever runners unknown: organization runner-group repository eligibility unavailable"
                                .to_owned(),
                        );
                        return None;
                    };
                    if !org_runners.is_empty() {
                        diagnostics.push(
                            "ci_queued_forever runners unknown: organization runner-group repository eligibility unavailable"
                                .to_owned(),
                        );
                        // Organization-level runners may be restricted to
                        // selected repositories. Without runner-group
                        // eligibility, their labels cannot safely prove that
                        // a queued job is unserved. Preserve unknown state so
                        // the condition cannot emit a false overdue alert.
                        return None;
                    }
                }
                Err(error)
                    if crate::head_approval::gh_error_http_status(error.message()) == Some(404) =>
                {
                    match actions.repository_owner_type(repo) {
                        Ok(owner_type) if owner_type.eq_ignore_ascii_case("User") => {}
                        Ok(owner_type) => {
                            diagnostics.push(format!(
                                "ci_queued_forever runners unknown: organization inventory returned 404 for owner type {owner_type}"
                            ));
                            return None;
                        }
                        Err(owner_error) => {
                            diagnostics.push(format!(
                                "ci_queued_forever runners unknown: owner type unavailable: {owner_error:?}"
                            ));
                            return None;
                        }
                    }
                }
                Err(error) => {
                    diagnostics.push(format!("ci_queued_forever runners unknown: {error:?}"));
                    return None;
                }
            }
            if let Some(labels) = online_runner_label_sets(&raw) {
                Some(labels)
            } else {
                diagnostics
                    .push("ci_queued_forever runners unknown: incomplete inventory".to_owned());
                None
            }
        }
        Err(error) => {
            diagnostics.push(format!("ci_queued_forever runners unknown: {error:?}"));
            None
        }
    }
}

fn job_check_run_id(job: &Value) -> Option<u64> {
    job.get("check_run_url")
        .and_then(Value::as_str)
        .and_then(|url| url.rsplit('/').next())
        .and_then(|id| id.parse().ok())
}

fn probe_billing(
    actions: &GitHubActions,
    repo: &str,
    default_branch: &str,
    now: DateTime<Utc>,
) -> (Option<CiHealthOverdue>, Vec<String>) {
    let mut errors = Vec::new();
    let mut successful_workflows = BTreeSet::new();

    match actions.list_runs_with_status(repo, "completed", Some(default_branch), MAX_BILLING_RUNS) {
        Ok(runs) => {
            for run in runs {
                // GitHub returns completed runs newest first. A successful
                // execution clears an older billing failure for the same
                // workflow, while failures in another workflow remain
                // independently observable.
                let workflow_key = if !run.path.is_empty() {
                    run.path.as_str()
                } else if !run.workflow_name.is_empty() {
                    run.workflow_name.as_str()
                } else {
                    run.name.as_str()
                };
                if successful_workflows.contains(workflow_key) {
                    continue;
                }
                if run.conclusion.as_deref() == Some("success") {
                    successful_workflows.insert(workflow_key.to_owned());
                    continue;
                }
                let jobs = match actions.workflow_run_jobs_raw(repo, run.database_id) {
                    Ok(jobs) => jobs,
                    Err(error) => {
                        errors.push(format!("ci_billing_blocked jobs: {error:?}"));
                        continue;
                    }
                };
                let Some(jobs) = jobs.get("jobs").and_then(Value::as_array) else {
                    errors.push("ci_billing_blocked jobs: response missing jobs array".to_owned());
                    continue;
                };
                for job in jobs
                    .iter()
                    .filter(|job| job.get("conclusion").and_then(Value::as_str) == Some("failure"))
                {
                    let Some(check_run_id) = job_check_run_id(job) else {
                        errors.push(
                            "ci_billing_blocked jobs: failed job missing check_run_url".to_owned(),
                        );
                        continue;
                    };
                    match actions.check_run_annotations_raw(repo, check_run_id) {
                        Ok(annotations) => {
                            let Some(annotations) = annotations.as_array() else {
                                errors.push(
                                    "ci_billing_blocked annotations: response is not an array"
                                        .to_owned(),
                                );
                                continue;
                            };
                            if annotations.iter().any(annotation_is_billing_blocked) {
                                return (
                                    Some(CiHealthOverdue {
                                        kind: "overdue",
                                        condition: "ci_billing_blocked",
                                        repo: repo.to_owned(),
                                        detail:
                                            "recent account payments have failed or spending limit"
                                                .to_owned(),
                                        at: now,
                                    }),
                                    errors,
                                );
                            }
                        }
                        Err(error) => {
                            errors.push(format!("ci_billing_blocked: {error:?}"));
                        }
                    }
                }
            }
        }
        Err(error) => errors.push(format!("ci_billing_blocked runs: {error:?}")),
    }
    (None, errors)
}

fn probe_queued(
    actions: &GitHubActions,
    repo: &str,
    now: DateTime<Utc>,
    errors: &mut Vec<String>,
) -> Option<CiHealthOverdue> {
    let online_labels = online_labels_or_unknown(actions, repo, errors)?;
    let mut runs = Vec::new();
    match actions.list_queued_runs(repo, MAX_QUEUED_RUNS) {
        Ok(queued) => runs.extend(queued),
        Err(error) => errors.push(format!("ci_queued_forever runs: {error:?}")),
    }
    match actions.list_runs_with_status(repo, "in_progress", None, MAX_QUEUED_RUNS) {
        Ok(in_progress) => runs.extend(in_progress),
        Err(error) => errors.push(format!("ci_queued_forever in-progress runs: {error:?}")),
    }
    for run in runs {
        let jobs = match actions.workflow_run_jobs_raw(repo, run.database_id) {
            Ok(jobs) => jobs,
            Err(error) => {
                errors.push(format!("ci_queued_forever jobs: {error:?}"));
                continue;
            }
        };
        let Some(jobs) = jobs.get("jobs").and_then(Value::as_array) else {
            errors.push("ci_queued_forever jobs: response missing jobs array".to_owned());
            continue;
        };
        for job in jobs {
            if job.get("status").and_then(Value::as_str) != Some("queued") {
                continue;
            }
            let Some(label_values) = job.get("labels").and_then(Value::as_array) else {
                errors.push("ci_queued_forever jobs: queued job missing labels array".to_owned());
                continue;
            };
            let mut labels = Vec::with_capacity(label_values.len());
            let mut malformed_labels = false;
            for value in label_values {
                if let Some(label) = value.as_str() {
                    labels.push(label.to_owned());
                } else {
                    malformed_labels = true;
                    break;
                }
            }
            if malformed_labels {
                errors.push("ci_queued_forever jobs: queued job has non-string label".to_owned());
                continue;
            }
            let queued_at = job
                .get("created_at")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(&run.created_at);
            if DateTime::parse_from_rfc3339(queued_at).is_err() {
                errors.push("ci_queued_forever jobs: queued job has invalid created_at".to_owned());
                continue;
            }
            if queued_job_is_forever_queued_on_runners(queued_at, &labels, &online_labels, now) {
                return Some(CiHealthOverdue {
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
            }
        }
    }
    None
}

/// Run both probes. Each failed read is returned as a loggable error and
/// produces no alert; a runners 403 is deliberately treated as unknown.
pub(super) fn probe(
    actions: &GitHubActions,
    repo: &str,
    default_branch: &str,
    now: DateTime<Utc>,
) -> (Vec<CiHealthOverdue>, Vec<String>) {
    let (billing, mut errors) = probe_billing(actions, repo, default_branch, now);
    let queued = probe_queued(actions, repo, now, &mut errors);
    (billing.into_iter().chain(queued).collect(), errors)
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
        assert!(queued_job_is_forever_queued(
            "2026-11-18T05:00:00Z",
            &["self-hosted".into()],
            &BTreeSet::new(),
            now
        ));
        assert!(!queued_job_is_forever_queued(
            "2026-11-18T05:00:00Z",
            &["self-hosted".into()],
            &served,
            now
        ));
        let split_runner_labels = vec![
            BTreeSet::from(["linux".to_owned(), "arm64".to_owned()]),
            BTreeSet::from(["macos".to_owned(), "x64".to_owned()]),
        ];
        assert!(queued_job_is_forever_queued_on_runners(
            "2026-11-18T05:00:00Z",
            &["self-hosted".into(), "macos".into(), "arm64".into()],
            &split_runner_labels,
            now
        ));
        assert!(!queued_job_is_forever_queued_on_runners(
            "2026-11-18T05:00:00Z",
            &["self-hosted".into(), "macos".into(), "x64".into()],
            &split_runner_labels,
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
        assert_eq!(
            online_runner_label_sets(&serde_json::json!({"runners": []})),
            None
        );
        assert_eq!(
            online_runner_label_sets(&serde_json::json!({
                "runners": [{"status": "offline", "labels": [{"name": "Retired"}]}]
            })),
            Some(Vec::new())
        );
        assert_eq!(
            online_runner_label_sets(&serde_json::json!({
                "runners": [{"status": "unknown", "labels": []}]
            })),
            None
        );
        assert!(queued_job_is_forever_queued_on_runners(
            old,
            &["self-hosted".to_owned(), "retired".to_owned()],
            &[],
            now
        ));
        assert_eq!(
            online_runner_label_sets(&serde_json::json!({
                "runners": [{"status": "online", "labels": [{"name": "Retired"}]}]
            })),
            Some(vec![BTreeSet::from(["retired".to_owned()])])
        );
    }

    #[test]
    fn feed_failure_does_not_commit_seen_state() {
        let mut seen = BTreeSet::from(["acme/repo:ci_billing_blocked".to_owned()]);
        let before = seen.clone();
        let rows = vec![CiHealthOverdue {
            kind: "overdue",
            condition: "ci_queued_forever",
            repo: "acme/repo".to_owned(),
            detail: "job build labels=self-hosted,retired".to_owned(),
            at: Utc::now(),
        }];
        let successful = BTreeSet::from(["ci_queued_forever"]);
        let result = commit_seen_after_feed(&mut seen, "acme/repo", &rows, &successful, |_| {
            Err("feed unavailable".to_owned())
        });
        assert_eq!(result, Err("feed unavailable".to_owned()));
        assert_eq!(seen, before);
    }

    #[test]
    fn successful_condition_updates_even_when_another_probe_failed() {
        let mut seen = BTreeSet::from([
            "acme/repo:ci_billing_blocked".to_owned(),
            "acme/repo:ci_queued_forever".to_owned(),
        ]);
        let rows = vec![CiHealthOverdue {
            kind: "overdue",
            condition: "ci_billing_blocked",
            repo: "acme/repo".to_owned(),
            detail: "recent account payments have failed or spending limit".to_owned(),
            at: Utc::now(),
        }];
        let successful = BTreeSet::from(["ci_billing_blocked"]);
        // The queued inventory read failed, so its prior state remains while
        // the independently successful billing probe is still recorded.
        commit_seen_after_feed(&mut seen, "acme/repo", &rows, &successful, |_| Ok(())).unwrap();
        assert!(seen.contains("acme/repo:ci_billing_blocked"));
        assert!(seen.contains("acme/repo:ci_queued_forever"));
    }

    #[test]
    fn positive_condition_is_recorded_despite_partial_probe_error() {
        let mut seen = BTreeSet::from(["acme/repo:ci_queued_forever".to_owned()]);
        let rows = vec![CiHealthOverdue {
            kind: "overdue",
            condition: "ci_billing_blocked",
            repo: "acme/repo".to_owned(),
            detail: "recent account payments have failed or spending limit".to_owned(),
            at: Utc::now(),
        }];
        let appended = std::cell::Cell::new(0);
        commit_seen_after_feed(
            &mut seen,
            "acme/repo",
            &rows,
            &BTreeSet::new(),
            |new_rows| {
                appended.set(new_rows.len());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(appended.get(), 1);
        assert!(seen.contains("acme/repo:ci_billing_blocked"));
        assert!(seen.contains("acme/repo:ci_queued_forever"));
    }

    #[cfg(unix)]
    #[test]
    fn organization_inventory_paginates_and_preserves_unknown_eligibility() {
        let temp = tempfile::tempdir().unwrap();
        let gh = temp.path().join("gh");
        let page_one = serde_json::to_string(&serde_json::json!({
            "runners": (0..100)
                .map(|index| serde_json::json!({
                    "status": "online",
                    "labels": [{"name": format!("org-page-one-{index}")}]
                }))
                .collect::<Vec<_>>()
        }))
        .unwrap();
        let page_two = serde_json::to_string(&serde_json::json!({
            "runners": [{
                "status": "online",
                "labels": [{"name": "org-only"}]
            }]
        }))
        .unwrap();
        let repository = serde_json::to_string(&serde_json::json!({"runners": []})).unwrap();
        let script = format!(
            "#!/bin/sh\nset -eu\ncase \"$*\" in\n  *\"repos/acme/repo/actions/runners?per_page=100\") printf '%s' '{repository}';;\n  *\"orgs/acme/actions/runners?per_page=100&page=2\") printf '%s' '{page_two}';;\n  *\"orgs/acme/actions/runners?per_page=100\") printf '%s' '{page_one}';;\n  *) echo \"unexpected args: $*\" >&2; exit 1;;\nesac\n"
        );
        crate::test_support::write_executable_script(&gh, &script);
        let actions = GitHubActions::hermetic_for_tests(temp.path(), &gh);
        let mut diagnostics = Vec::new();
        assert!(online_labels_or_unknown(&actions, "acme/repo", &mut diagnostics).is_none());
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("repository eligibility unavailable"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn paginated_organization_label_reaches_queue_probe() {
        let temp = tempfile::tempdir().unwrap();
        let gh = temp.path().join("gh");
        let page_one = serde_json::to_string(&serde_json::json!({
            "runners": (0..100)
                .map(|index| serde_json::json!({
                    "status": "online",
                    "labels": [{"name": format!("org-page-one-{index}")}]
                }))
                .collect::<Vec<_>>()
        }))
        .unwrap();
        let page_two = serde_json::to_string(&serde_json::json!({
            "runners": [{
                "status": "online",
                "labels": [{"name": "org-only"}]
            }]
        }))
        .unwrap();
        let script = format!(
            "#!/bin/sh\nset -eu\ncase \"$*\" in\n  *\"actions/runs?status=completed&per_page=5&branch=main\") printf '%s' '{{\"workflow_runs\":[]}}';;\n  *\"repos/acme/repo/actions/runners?per_page=100\") printf '%s' '{{\"runners\":[]}}';;\n  *\"orgs/acme/actions/runners?per_page=100&page=2\") printf '%s' '{page_two}';;\n  *\"orgs/acme/actions/runners?per_page=100\") printf '%s' '{page_one}';;\n  *\"actions/runs?status=queued&per_page=5\") printf '%s' '{{\"workflow_runs\":[{{\"id\":42,\"name\":\"CI\",\"head_branch\":\"main\",\"event\":\"push\",\"created_at\":\"2026-11-18T00:00:00Z\"}}]}}';;\n  *\"actions/runs?status=in_progress&per_page=5\") printf '%s' '{{\"workflow_runs\":[]}}';;\n  *\"actions/runs/42/jobs?per_page=100\") printf '%s' '{{\"jobs\":[{{\"status\":\"queued\",\"name\":\"build\",\"created_at\":\"2026-11-18T00:00:00Z\",\"labels\":[\"self-hosted\",\"org-only\"]}}]}}';;\n  *) echo \"unexpected args: $*\" >&2; exit 1;;\nesac\n"
        );
        crate::test_support::write_executable_script(&gh, &script);
        let actions = GitHubActions::hermetic_for_tests(temp.path(), &gh);
        let now = DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (rows, errors) = probe(&actions, "acme/repo", "main", now);
        assert!(rows.is_empty(), "{rows:?}");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("repository eligibility unavailable"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn organization_inventory_404_is_unknown_unless_owner_is_personal() {
        let temp = tempfile::tempdir().unwrap();
        let gh = temp.path().join("gh");
        let repository_runners = serde_json::to_string(&serde_json::json!({
            "runners": [{
                "status": "online",
                "labels": [{"name": "repo-only"}]
            }]
        }))
        .unwrap();
        let script = format!(
            "#!/bin/sh\nset -eu\ncase \"$*\" in\n  *\"repos/acme/repo/actions/runners?per_page=100\") printf '%s' '{repository_runners}';;\n  *\"orgs/acme/actions/runners?per_page=100\") echo 'gh: Not Found (HTTP 404)' >&2; exit 1;;\n  *\"repos/acme/repo\") printf '%s' '{{\"owner\":{{\"type\":\"Organization\"}}}}';;\n  *) echo \"unexpected args: $*\" >&2; exit 1;;\nesac\n"
        );
        crate::test_support::write_executable_script(&gh, &script);
        let actions = GitHubActions::hermetic_for_tests(temp.path(), &gh);
        let mut diagnostics = Vec::new();
        assert!(online_labels_or_unknown(&actions, "acme/repo", &mut diagnostics).is_none());
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("owner type Organization"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn personal_repository_inventory_404_falls_back_to_repository_runners() {
        let temp = tempfile::tempdir().unwrap();
        let gh = temp.path().join("gh");
        let repository_runners = serde_json::to_string(&serde_json::json!({
            "runners": [{
                "status": "online",
                "labels": [{"name": "personal-only"}]
            }]
        }))
        .unwrap();
        let script = format!(
            "#!/bin/sh\nset -eu\ncase \"$*\" in\n  *\"repos/acme/repo/actions/runners?per_page=100\") printf '%s' '{repository_runners}';;\n  *\"orgs/acme/actions/runners?per_page=100\") echo 'gh: Not Found (HTTP 404)' >&2; exit 1;;\n  *\"repos/acme/repo\") printf '%s' '{{\"owner\":{{\"type\":\"User\"}}}}';;\n  *) echo \"unexpected args: $*\" >&2; exit 1;;\nesac\n"
        );
        crate::test_support::write_executable_script(&gh, &script);
        let actions = GitHubActions::hermetic_for_tests(temp.path(), &gh);
        let mut diagnostics = Vec::new();
        let online = online_labels_or_unknown(&actions, "acme/repo", &mut diagnostics)
            .expect("personal repository should use repository inventory");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(online[0].contains("personal-only"));
    }

    #[cfg(unix)]
    #[test]
    fn in_progress_run_with_queued_job_is_checked_for_unserved_labels() {
        let temp = tempfile::tempdir().unwrap();
        let gh = temp.path().join("gh");
        let script = r#"#!/bin/sh
set -eu
case "$*" in
  *"actions/runs?status=completed&per_page=5&branch=main") printf '%s' '{"workflow_runs":[]}';;
  *"orgs/acme/actions/runners?per_page=100") echo 'gh: Not Found (HTTP 404)' >&2; exit 1;;
  *"repos/acme/repo/actions/runners?per_page=100") printf '%s' '{"runners":[{"status":"online","labels":[{"name":"linux"}]}]}';;
  *"repos/acme/repo" ) printf '%s' '{"owner":{"type":"User"}}';;
  *"actions/runs?status=queued&per_page=5") printf '%s' '{"workflow_runs":[]}';;
  *"actions/runs?status=in_progress&per_page=5") printf '%s' '{"workflow_runs":[{"id":42,"name":"CI","head_branch":"main","event":"push","created_at":"2026-11-18T00:00:00Z"}]}';;
  *"actions/runs/42/jobs?per_page=100") printf '%s' '{"jobs":[{"status":"queued","name":"build","created_at":"2026-11-18T00:00:00Z","labels":["self-hosted","retired"]}]}';;
  *) echo "unexpected args: $*" >&2; exit 1;;
esac
"#;
        crate::test_support::write_executable_script(&gh, script);
        let actions = GitHubActions::hermetic_for_tests(temp.path(), &gh);
        let now = DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (rows, errors) = probe(&actions, "acme/repo", "main", now);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].condition, "ci_queued_forever");
    }

    #[cfg(unix)]
    #[test]
    fn billing_probe_checks_every_failed_job_for_annotations() {
        let temp = tempfile::tempdir().unwrap();
        let gh = temp.path().join("gh");
        let script = r#"#!/bin/sh
set -eu
case "$*" in
  *"actions/runs?status=completed&per_page=5&branch=main") printf '%s' '{"workflow_runs":[{"id":8,"name":"Hosted","head_branch":"main","event":"push","created_at":"2026-11-18T11:00:00Z"},{"id":7,"name":"Self-hosted","head_branch":"main","event":"push","created_at":"2026-11-18T00:00:00Z"}]}';;
  *"actions/runs/8/jobs?per_page=100") printf '%s' '{"jobs":[]}';;
  *"actions/runs/7/jobs?per_page=100") printf '%s' '{"jobs":[{"id":1,"check_run_url":"https://api.github.com/repos/acme/repo/check-runs/101","conclusion":"failure"},{"id":2,"check_run_url":"https://api.github.com/repos/acme/repo/check-runs/202","conclusion":"failure"}]}';;
  *"check-runs/101/annotations?per_page=30") printf '%s' '[]';;
  *"check-runs/202/annotations?per_page=30") printf '%s' '[{"message":"spending limit reached"}]';;
  *"orgs/acme/actions/runners?per_page=100") echo 'gh: Not Found (HTTP 404)' >&2; exit 1;;
  *"repos/acme/repo/actions/runners?per_page=100") printf '%s' '{"runners":[{"status":"online","labels":[{"name":"linux"}]}]}';;
  *"repos/acme/repo") printf '%s' '{"owner":{"type":"User"}}';;
  *"actions/runs?status=queued&per_page=5") printf '%s' '{"workflow_runs":[]}';;
  *"actions/runs?status=in_progress&per_page=5") printf '%s' '{"workflow_runs":[]}';;
  *) echo "unexpected args: $*" >&2; exit 1;;
esac
"#;
        crate::test_support::write_executable_script(&gh, script);
        let actions = GitHubActions::hermetic_for_tests(temp.path(), &gh);
        let now = DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (rows, errors) = probe(&actions, "acme/repo", "main", now);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].condition, "ci_billing_blocked");
    }

    #[cfg(unix)]
    #[test]
    fn billing_failure_is_cleared_by_a_newer_successful_run() {
        let temp = tempfile::tempdir().unwrap();
        let gh = temp.path().join("gh");
        let script = r#"#!/bin/sh
set -eu
case "$*" in
  *"actions/runs?status=completed&per_page=5&branch=main") printf '%s' '{"workflow_runs":[{"id":8,"name":"CI","path":".github/workflows/ci.yml","conclusion":"success","created_at":"2026-11-18T11:00:00Z"},{"id":7,"name":"CI","path":".github/workflows/ci.yml","conclusion":"failure","created_at":"2026-11-18T00:00:00Z"}]}';;
  *"actions/runs/7/jobs?per_page=100") printf '%s' '{"jobs":[{"check_run_url":"https://api.github.com/repos/acme/repo/check-runs/101","conclusion":"failure"}]}';;
  *"check-runs/101/annotations?per_page=30") printf '%s' '[{"message":"spending limit reached"}]';;
  *"orgs/acme/actions/runners?per_page=100") echo 'gh: Not Found (HTTP 404)' >&2; exit 1;;
  *"repos/acme/repo/actions/runners?per_page=100") printf '%s' '{"runners":[{"status":"online","labels":[{"name":"linux"}]}]}';;
  *"repos/acme/repo") printf '%s' '{"owner":{"type":"User"}}';;
  *"actions/runs?status=queued&per_page=5") printf '%s' '{"workflow_runs":[]}';;
  *"actions/runs?status=in_progress&per_page=5") printf '%s' '{"workflow_runs":[]}';;
  *) echo "unexpected args: $*" >&2; exit 1;;
esac
"#;
        crate::test_support::write_executable_script(&gh, script);
        let actions = GitHubActions::hermetic_for_tests(temp.path(), &gh);
        let now = DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (rows, errors) = probe(&actions, "acme/repo", "main", now);
        assert!(rows.is_empty(), "{rows:?}");
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[cfg(unix)]
    #[test]
    fn organization_runner_group_scope_keeps_availability_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let gh = temp.path().join("gh");
        let script = r#"#!/bin/sh
set -eu
case "$*" in
  *"repos/acme/repo/actions/runners?per_page=100") printf '%s' '{"runners":[]}';;
  *"orgs/acme/actions/runners?per_page=100") printf '%s' '{"runners":[{"status":"online","runner_group_id":7,"labels":[{"name":"restricted"}]}]}';;
  *) echo "unexpected args: $*" >&2; exit 1;;
esac
"#;
        crate::test_support::write_executable_script(&gh, script);
        let actions = GitHubActions::hermetic_for_tests(temp.path(), &gh);
        let mut diagnostics = Vec::new();
        assert!(online_labels_or_unknown(&actions, "acme/repo", &mut diagnostics).is_none());
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("repository eligibility unavailable"))
        );
    }
}
