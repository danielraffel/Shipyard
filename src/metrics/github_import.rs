//! The GitHub Actions job import shared by `shipyard metrics import github`
//! and the daemon's scheduled `[metrics.import]` job.
//!
//! The caller supplies the `gh api` runner, so the CLI keeps its bounded
//! subprocess (and its exit-code mapping) while the daemon uses its configured
//! GitHub client.

use serde_json::Value;
use std::collections::HashSet;

use super::{GitHubRunJob, GithubRunMetadata, MetricsStore, github_job_to_record_with_run};

/// What to import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubImportRequest {
    /// `owner/name`.
    pub repo: String,
    /// Project key; defaults to the repository name.
    pub project: Option<String>,
    /// Workflow file or id; every workflow when `None`.
    pub workflow: Option<String>,
    /// Branch filter.
    pub branch: Option<String>,
    /// Recent runs to read.
    pub limit: u32,
}

impl GithubImportRequest {
    /// The project key rows are stored under.
    #[must_use]
    pub fn project_key(&self) -> String {
        self.project.clone().unwrap_or_else(|| {
            self.repo
                .rsplit('/')
                .next()
                .unwrap_or(&self.repo)
                .to_owned()
        })
    }
}

/// `gh api` path listing workflow runs.
#[must_use]
pub fn github_runs_api_path(repo: &str, workflow: Option<&str>) -> String {
    workflow.map_or_else(
        || format!("/repos/{repo}/actions/runs"),
        |workflow| format!("/repos/{repo}/actions/workflows/{workflow}/runs"),
    )
}

/// `gh api` path listing one run's jobs.
#[must_use]
pub fn github_jobs_api_path(repo: &str, run_id: i64) -> String {
    format!("/repos/{repo}/actions/runs/{run_id}/jobs")
}

/// A run's pull request, only when it names exactly one.
#[must_use]
pub fn workflow_run_single_pr(run: &Value) -> Option<i64> {
    let pull_requests = run.get("pull_requests")?.as_array()?;
    let [pull_request] = pull_requests.as_slice() else {
        return None;
    };
    pull_request.get("number").and_then(Value::as_i64)
}

/// Import completed jobs from the most recent runs. `gh` runs one `gh` argv
/// and returns its parsed JSON; `store_error` turns a store/parse failure into
/// the caller's error type. Returns the number of job rows observed.
///
/// # Errors
/// Whatever `gh` returns, or a store/parse failure.
#[allow(clippy::too_many_lines)]
pub fn import_github<E>(
    store: &MetricsStore,
    request: &GithubImportRequest,
    gh: &mut dyn FnMut(&[String]) -> Result<Value, E>,
    store_error: &dyn Fn(String) -> E,
) -> Result<usize, E> {
    let mut run_args = vec![
        "api".to_owned(),
        "-X".to_owned(),
        "GET".to_owned(),
        github_runs_api_path(&request.repo, request.workflow.as_deref()),
        "-f".to_owned(),
        format!("per_page={}", request.limit),
    ];
    if let Some(branch) = &request.branch {
        run_args.push("-f".to_owned());
        run_args.push(format!("branch={branch}"));
    }
    let runs = gh(&run_args)?;
    let Some(run_values) = runs.get("workflow_runs").and_then(Value::as_array) else {
        return Err(store_error(
            "GitHub workflow_runs array is missing or malformed".to_owned(),
        ));
    };
    // `limit` is an intentional bounded sample. A larger provider
    // `total_count` therefore does not make this response malformed; the
    // caller's imported count remains a bounded sample rather than a census.
    let mut run_ids = Vec::with_capacity(run_values.len());
    for run in run_values {
        let metadata = serde_json::from_value::<GithubRunMetadata>(run.clone())
            .map_err(|error| store_error(format!("GitHub workflow run parse failed: {error}")))?;
        let Some(run_id) = metadata.id else {
            return Err(store_error(
                "GitHub workflow run has no numeric id".to_owned(),
            ));
        };
        let Some(event) = metadata.event.as_deref().filter(|event| !event.is_empty()) else {
            return Err(store_error(format!(
                "GitHub workflow run {run_id} has no event"
            )));
        };
        if event == "merge_group" && metadata.head_sha.as_deref().is_none_or(str::is_empty) {
            return Err(store_error(format!(
                "GitHub merge_group run {run_id} has no head_sha"
            )));
        }
        run_ids.push((run_id, workflow_run_single_pr(run), metadata));
    }
    let project = request.project_key();
    let mut pending_inputs = Vec::new();
    for (run_id, pr, metadata) in run_ids {
        let mut page = 1u32;
        let mut observed = 0u64;
        let mut seen_job_ids = HashSet::new();
        loop {
            if page > 100 {
                return Err(store_error(format!(
                    "GitHub jobs pagination exceeded 100 pages for run {run_id}"
                )));
            }
            let jobs = gh(&[
                "api".to_owned(),
                "-X".to_owned(),
                "GET".to_owned(),
                github_jobs_api_path(&request.repo, run_id),
                "-f".to_owned(),
                "per_page=100".to_owned(),
                "-f".to_owned(),
                "filter=all".to_owned(),
                "-f".to_owned(),
                format!("page={page}"),
            ])?;
            let Some(job_values) = jobs.get("jobs").and_then(Value::as_array) else {
                return Err(store_error(format!(
                    "GitHub jobs array is missing or malformed for run {run_id}"
                )));
            };
            let total = jobs.get("total_count").and_then(Value::as_u64);
            if total.is_none() && job_values.len() >= 100 {
                return Err(store_error(format!(
                    "GitHub jobs response may be truncated for run {run_id}"
                )));
            }
            for value in job_values {
                let mut job: GitHubRunJob = serde_json::from_value(value.clone())
                    .map_err(|error| store_error(format!("GitHub job parse failed: {error}")))?;
                if !seen_job_ids.insert(job.id) {
                    return Err(store_error(format!(
                        "GitHub jobs response repeated job {} for run {run_id}",
                        job.id
                    )));
                }
                job.run_id.get_or_insert(run_id);
                if job.run_id != Some(run_id) {
                    return Err(store_error(format!(
                        "GitHub job {} is bound to contradictory run id",
                        job.id
                    )));
                }
                if let (Some(run_head), Some(job_head)) =
                    (metadata.head_sha.as_deref(), job.head_sha.as_deref())
                    && run_head != job_head
                {
                    return Err(store_error(format!(
                        "GitHub job {} is bound to contradictory head sha",
                        job.id
                    )));
                }
                if job.completed_at.is_none() {
                    continue;
                }
                let input = github_job_to_record_with_run(
                    &request.repo,
                    request.workflow.as_deref(),
                    &project,
                    pr,
                    &job,
                    &metadata,
                );
                pending_inputs.push(input);
            }
            observed = observed.saturating_add(job_values.len() as u64);
            match total {
                None if job_values.len() < 100 => break,
                Some(total) if observed >= total => break,
                Some(total) if job_values.len() < 100 => {
                    return Err(store_error(format!(
                        "GitHub jobs response ended at {observed} of {total} for run {run_id}"
                    )));
                }
                _ => {}
            }
            page = page.saturating_add(1);
        }
    }
    if pending_inputs.is_empty() {
        return Ok(0);
    }
    store
        .record_terminal_observations(&pending_inputs)
        .map_err(|error| store_error(format!("GitHub metrics record failed: {error}")))?;
    Ok(pending_inputs.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(clippy::type_complexity)]
    #[test]
    fn imports_completed_jobs_through_the_supplied_runner() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let request = GithubImportRequest {
            repo: "Generous-Corp/pulp".to_owned(),
            project: None,
            workflow: Some("build.yml".to_owned()),
            branch: None,
            limit: 5,
        };
        let mut calls = Vec::new();
        let mut gh = |argv: &[String]| -> Result<Value, String> {
            calls.push(argv[3].clone());
            if argv[3].ends_with("/runs") {
                Ok(json!({"workflow_runs": [{"id": 7, "run_attempt": 2,
                    "event": "pull_request", "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "head_branch": "feature/test", "pull_requests": [{"number": 9}]}]}))
            } else {
                Ok(json!({"jobs": [
                    {"id": 1, "run_attempt": 2, "name": "macos", "conclusion": "success",
                     "started_at": "2026-09-30T10:00:00Z", "completed_at": "2026-09-30T10:10:00Z",
                     "labels": ["self-hosted"]},
                    {"id": 2, "name": "linux", "status": "in_progress"}
                ]}))
            }
        };
        let imported = import_github(&store, &request, &mut gh, &|error| error).unwrap();
        assert_eq!(imported, 1);
        assert_eq!(
            calls,
            vec![
                "/repos/Generous-Corp/pulp/actions/workflows/build.yml/runs".to_owned(),
                "/repos/Generous-Corp/pulp/actions/runs/7/jobs".to_owned(),
            ]
        );
        let rows = store.list(Some("Generous-Corp/pulp"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project, "pulp");
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        let identity: (
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT event, provider_run_id, run_attempt, head_sha, provenance FROM runs",
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
        assert_eq!(identity.0.as_deref(), Some("pull_request"));
        assert_eq!(identity.1, Some(7));
        assert_eq!(identity.2, Some(2));
        assert_eq!(
            identity.3.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(identity.4, None);
    }

    #[test]
    fn rerun_import_refreshes_unknown_attempt_from_production_provider() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let request = GithubImportRequest {
            repo: "acme/pulp".to_owned(),
            project: None,
            workflow: Some("build.yml".to_owned()),
            branch: None,
            limit: 5,
        };
        let run = json!({"id": 7, "event": "pull_request", "run_attempt": 2,
            "head_sha": "head", "head_branch": "feature/test",
            "pull_requests": [{"number": 9}]});
        let mut first = true;
        let mut gh = |argv: &[String]| -> Result<Value, String> {
            if argv[3].ends_with("/runs") {
                return Ok(json!({"workflow_runs": [run.clone()]}));
            }
            if first {
                first = false;
                Ok(json!({"jobs": [{"id": 1, "name": "macos",
                    "conclusion": "success", "completed_at": "2026-09-30T10:10:00Z"}]}))
            } else {
                Ok(json!({"jobs": [{"id": 1, "run_attempt": 2, "name": "macos",
                    "conclusion": "success", "completed_at": "2026-09-30T10:11:00Z"}]}))
            }
        };
        import_github(&store, &request, &mut gh, &|error| error).unwrap();
        import_github(&store, &request, &mut gh, &|error| error).unwrap();
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM jobs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT run_attempt FROM runs", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn missing_run_array_is_rejected_without_inventing_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let request = GithubImportRequest {
            repo: "acme/pulp".to_owned(),
            project: None,
            workflow: None,
            branch: None,
            limit: 5,
        };
        let mut gh = |_argv: &[String]| -> Result<Value, String> { Ok(json!({})) };
        assert!(import_github(&store, &request, &mut gh, &|error| error).is_err());
    }

    #[allow(clippy::single_element_loop)]
    #[test]
    fn rejects_missing_jobs_and_contradictory_head() {
        let request = GithubImportRequest {
            repo: "acme/pulp".to_owned(),
            project: None,
            workflow: None,
            branch: None,
            limit: 5,
        };
        let run = json!({"workflow_runs": [{"id": 7, "run_attempt": 2,
            "event": "pull_request", "head_sha": "head", "pull_requests": [{"number": 9}]}]});
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let mut gh = |argv: &[String]| -> Result<Value, String> {
            if argv[3].ends_with("/runs") {
                Ok(run.clone())
            } else {
                Ok(json!({}))
            }
        };
        assert!(import_github(&store, &request, &mut gh, &|error| error).is_err());

        for (field, value) in [("head_sha", json!("other"))] {
            let dir = tempfile::tempdir().unwrap();
            let store = MetricsStore::open(dir.path()).unwrap();
            let mut gh = |argv: &[String]| -> Result<Value, String> {
                if argv[3].ends_with("/runs") {
                    Ok(run.clone())
                } else {
                    Ok(
                        json!({"jobs": [{"id": 1, "run_attempt": if field == "run_attempt" { value.clone() } else { json!(2) },
                        "head_sha": if field == "head_sha" { value.clone() } else { json!("head") },
                        "name": "macos", "conclusion": "success", "completed_at": "2026-09-30T10:10:00Z"}]}),
                    )
                }
            };
            assert!(import_github(&store, &request, &mut gh, &|error| error).is_err());
        }
    }

    #[test]
    fn rejects_potentially_truncated_jobs_without_pagination_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let request = GithubImportRequest {
            repo: "acme/pulp".to_owned(),
            project: None,
            workflow: None,
            branch: None,
            limit: 5,
        };
        let mut gh = |argv: &[String]| -> Result<Value, String> {
            if argv[3].ends_with("/runs") {
                Ok(json!({"total_count": 999, "workflow_runs": [{"id": 7,
                    "event": "pull_request", "run_attempt": 1, "head_sha": "head",
                    "pull_requests": [{"number": 9}]}]}))
            } else {
                let jobs = (0..100)
                    .map(|id| {
                        json!({"id": id, "name": "job",
                    "conclusion": "success", "completed_at": "2026-09-30T10:10:00Z"})
                    })
                    .collect::<Vec<_>>();
                Ok(json!({"jobs": jobs}))
            }
        };
        assert!(import_github(&store, &request, &mut gh, &|error| error).is_err());
    }

    #[test]
    fn imports_all_jobs_across_multiple_pages() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let request = GithubImportRequest {
            repo: "acme/pulp".to_owned(),
            project: None,
            workflow: None,
            branch: None,
            limit: 5,
        };
        let mut pages = Vec::new();
        let mut gh = |argv: &[String]| -> Result<Value, String> {
            if argv[3].ends_with("/runs") {
                return Ok(json!({"workflow_runs": [{"id": 7, "event": "pull_request",
                    "run_attempt": 2, "head_sha": "head",
                    "pull_requests": [{"number": 9}]}]}));
            }
            let page = if argv.iter().any(|arg| arg == "page=2") {
                2
            } else {
                1
            };
            pages.push(page);
            let start = if page == 1 { 0 } else { 100 };
            let end = if page == 1 { 100 } else { 150 };
            let jobs = (start..end)
                .map(|id| {
                    json!({"id": id, "name": format!("job-{id}"),
                    "run_attempt": 2, "conclusion": "success",
                    "completed_at": "2026-09-30T10:10:00Z"})
                })
                .collect::<Vec<_>>();
            Ok(json!({"total_count": 150, "jobs": jobs}))
        };
        assert_eq!(
            import_github(&store, &request, &mut gh, &|error| error).unwrap(),
            150
        );
        assert_eq!(pages, vec![1, 2]);
        assert_eq!(store.list(Some("acme/pulp"), 200).unwrap().len(), 150);
    }

    #[test]
    fn rejects_short_page_before_total_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let request = GithubImportRequest {
            repo: "acme/pulp".to_owned(),
            project: None,
            workflow: None,
            branch: None,
            limit: 5,
        };
        let mut gh = |argv: &[String]| -> Result<Value, String> {
            if argv[3].ends_with("/runs") {
                return Ok(json!({"workflow_runs": [{"id": 7, "event": "pull_request",
                    "run_attempt": 2, "head_sha": "head",
                    "pull_requests": [{"number": 9}]}]}));
            }
            let jobs = (0..50)
                .map(|id| {
                    json!({"id": id, "name": "job", "run_attempt": 2,
                    "conclusion": "success", "completed_at": "2026-09-30T10:10:00Z"})
                })
                .collect::<Vec<_>>();
            Ok(json!({"total_count": 150, "jobs": jobs}))
        };
        assert!(import_github(&store, &request, &mut gh, &|error| error).is_err());
        assert!(store.list(Some("acme/pulp"), 10).unwrap().is_empty());
    }

    #[test]
    fn keeps_job_attempt_from_job_object_on_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetricsStore::open(dir.path()).unwrap();
        let request = GithubImportRequest {
            repo: "acme/pulp".to_owned(),
            project: None,
            workflow: None,
            branch: None,
            limit: 5,
        };
        let mut gh = |argv: &[String]| -> Result<Value, String> {
            if argv[3].ends_with("/runs") {
                return Ok(json!({"workflow_runs": [{"id": 7, "event": "pull_request",
                    "run_attempt": 2, "head_sha": "head",
                    "pull_requests": [{"number": 9}]}]}));
            }
            Ok(json!({"jobs": [{"id": 1, "run_attempt": 1, "name": "macos",
                "conclusion": "success", "completed_at": "2026-09-30T10:10:00Z"}]}))
        };
        assert_eq!(
            import_github(&store, &request, &mut gh, &|error| error).unwrap(),
            1
        );
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        assert_eq!(
            conn.query_row("SELECT run_attempt FROM runs", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
