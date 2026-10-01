//! The GitHub Actions job import shared by `shipyard metrics import github`
//! and the daemon's scheduled `[metrics.import]` job.
//!
//! The caller supplies the `gh api` runner, so the CLI keeps its bounded
//! subprocess (and its exit-code mapping) while the daemon uses its configured
//! GitHub client.

use serde_json::Value;

use super::{GitHubRunJob, MetricsStore, github_job_to_record};

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
    let run_ids = runs
        .get("workflow_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|run| {
            let run_id = run.get("id").and_then(Value::as_i64)?;
            Some((run_id, workflow_run_single_pr(run)))
        })
        .collect::<Vec<_>>();
    let project = request.project_key();
    let mut imported = 0;
    for (run_id, pr) in run_ids {
        let jobs = gh(&[
            "api".to_owned(),
            "-X".to_owned(),
            "GET".to_owned(),
            github_jobs_api_path(&request.repo, run_id),
            "-f".to_owned(),
            "per_page=100".to_owned(),
        ])?;
        let Some(job_values) = jobs.get("jobs").and_then(Value::as_array) else {
            continue;
        };
        for value in job_values {
            let mut job: GitHubRunJob = serde_json::from_value(value.clone())
                .map_err(|error| store_error(format!("GitHub job parse failed: {error}")))?;
            job.run_id.get_or_insert(run_id);
            if job.completed_at.is_none() {
                continue;
            }
            let input = github_job_to_record(
                &request.repo,
                request.workflow.as_deref(),
                &project,
                pr,
                &job,
            );
            store
                .record_terminal_observation(&input)
                .map_err(|error| store_error(format!("GitHub metrics record failed: {error}")))?;
            imported += 1;
        }
    }
    Ok(imported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                Ok(json!({"workflow_runs": [{"id": 7, "pull_requests": [{"number": 9}]}]}))
            } else {
                Ok(json!({"jobs": [
                    {"id": 1, "name": "macos", "conclusion": "success",
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
    }
}
