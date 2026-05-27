//! GitHub Actions workflow discovery, dispatch planning, and `gh` helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use toml::Table;

use crate::config::LoadedConfig;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;

const RUN_JSON_FIELDS: &str =
    "databaseId,status,conclusion,url,createdAt,updatedAt,workflowName,headBranch,headSha";
const JOB_ACTIVE_STATUSES: [&str; 2] = ["queued", "in_progress"];

/// A workflow that can be launched with `workflow_dispatch`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowDefinition {
    /// Stable local key, derived from the workflow filename or an alias.
    pub key: String,
    /// GitHub workflow filename.
    pub file: String,
    /// Display name from the workflow file.
    pub name: String,
    /// Human-readable summary for CLI rendering.
    pub description: String,
    /// Declared `workflow_dispatch` input names.
    pub inputs: Vec<String>,
}

/// Resolved GitHub Actions dispatch settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CloudDispatchPlan {
    /// Workflow selected for dispatch.
    pub workflow: WorkflowDefinition,
    /// Optional configured repository override.
    pub repository: Option<String>,
    /// Git ref/branch to dispatch.
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// Resolved runner provider.
    pub provider: String,
    /// `workflow_dispatch` fields to send to GitHub.
    pub dispatch_fields: BTreeMap<String, String>,
    /// Where important plan values came from.
    pub sources: BTreeMap<String, String>,
}

/// CLI-supplied dispatch overrides layered on top of config/provider defaults.
#[derive(Clone, Copy, Debug, Default)]
pub struct CloudDispatchOverrides<'a> {
    /// Runner provider override, matching `shipyard cloud run --provider`.
    pub provider: Option<&'a str>,
    /// Generic runner selector override.
    pub runner_selector: Option<&'a str>,
    /// Linux runner selector override.
    pub linux_runner_selector: Option<&'a str>,
    /// Windows runner selector override.
    pub windows_runner_selector: Option<&'a str>,
    /// macOS runner selector override.
    pub macos_runner_selector: Option<&'a str>,
}

impl<'a> CloudDispatchOverrides<'a> {
    /// Build overrides that only carry the provider override.
    #[must_use]
    pub fn provider(provider: Option<&'a str>) -> Self {
        Self {
            provider,
            ..Self::default()
        }
    }

    fn platform_selectors(self) -> BTreeMap<&'static str, &'a str> {
        [
            ("linux-x64", self.linux_runner_selector),
            ("windows-x64", self.windows_runner_selector),
            ("macos-arm64", self.macos_runner_selector),
        ]
        .into_iter()
        .filter_map(|(platform, selector)| selector.map(|selector| (platform, selector)))
        .collect()
    }
}

/// Minimal GitHub Actions workflow run shape used by cloud commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowRun {
    /// GitHub database ID for the run.
    pub database_id: u64,
    /// Current GitHub Actions status.
    pub status: String,
    /// Terminal conclusion, when present.
    pub conclusion: Option<String>,
    /// HTML URL, when returned by `gh`.
    pub url: Option<String>,
}

/// Minimal GitHub Actions job shape used by retargeting.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkflowJob {
    /// GitHub database ID for the job.
    pub database_id: u64,
    /// GitHub Actions job name.
    pub name: String,
}

/// Queued workflow run returned by the GitHub Actions API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedRun {
    /// GitHub database ID for the run.
    pub database_id: u64,
    /// Workflow/run display name.
    pub name: String,
    /// Branch associated with the run.
    pub head_branch: String,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// Workflow display name.
    pub workflow_name: String,
    /// HTML URL.
    pub url: Option<String>,
    /// Workflow path, e.g. `.github/workflows/ci.yml`.
    pub path: String,
    /// GitHub Actions status (`queued`, `in_progress`, `completed`, ...).
    pub status: String,
    /// Terminal conclusion when the run is completed.
    pub conclusion: Option<String>,
}

/// Metadata for a single workflow run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunMetadata {
    /// Workflow file path.
    pub path: String,
    /// Branch associated with the run.
    pub head_branch: String,
    /// Workflow/run display name.
    pub name: String,
    /// GitHub Actions status.
    pub status: String,
}

/// Error returned by GitHub Actions helper operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubError {
    message: String,
}

impl GitHubError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    fn command_failed(args: &[String], status: Option<i32>, stderr: &[u8]) -> Self {
        let stderr = String::from_utf8_lossy(stderr).trim().to_owned();
        let status = status.map_or_else(|| "signal".to_owned(), |code| code.to_string());
        Self::new(format!(
            "gh {} failed with status {status}: {stderr}",
            args.join(" ")
        ))
    }
}

impl Display for GitHubError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for GitHubError {}

/// Shell-backed GitHub Actions client.
#[derive(Clone, Debug)]
pub struct GitHubActions {
    cwd: PathBuf,
    gh: Result<GhClient, String>,
    gh_binary_override: Option<PathBuf>,
}

impl PartialEq for GitHubActions {
    fn eq(&self, other: &Self) -> bool {
        self.cwd == other.cwd
    }
}

impl Eq for GitHubActions {}

impl GitHubActions {
    /// Build a client that runs `gh` from `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::from_cwd(RuntimeMode::Shipyard, cwd)
    }

    /// Build a client that runs `gh` from `cwd` using the requested runtime mode.
    #[must_use]
    pub fn from_cwd(mode: RuntimeMode, cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let gh = GhClient::from_cwd(mode, &cwd)
            .map_err(|error| format!("failed to load GitHub auth config for gh commands: {error}"));
        Self {
            cwd,
            gh,
            gh_binary_override: None,
        }
    }

    /// Build a client from an already loaded Shipyard config.
    #[must_use]
    pub fn from_loaded_config(cwd: impl Into<PathBuf>, config: &LoadedConfig) -> Self {
        let cwd = cwd.into();
        let gh = GhClient::from_loaded_config(config)
            .map_err(|error| format!("failed to load GitHub auth config for gh commands: {error}"));
        Self {
            cwd,
            gh,
            gh_binary_override: None,
        }
    }

    #[cfg(all(test, unix))]
    fn with_gh_binary_for_tests(mut self, gh_binary: impl Into<PathBuf>) -> Self {
        self.gh_binary_override = Some(gh_binary.into());
        self
    }

    /// Dispatch a workflow with optional repository and input fields.
    pub fn workflow_dispatch(
        &self,
        repository: Option<&str>,
        workflow_file: &str,
        ref_name: &str,
        fields: &BTreeMap<String, String>,
    ) -> Result<(), GitHubError> {
        let mut args = vec![
            "workflow".to_owned(),
            "run".to_owned(),
            workflow_file.to_owned(),
            "--ref".to_owned(),
            ref_name.to_owned(),
        ];
        if let Some(repository) = repository {
            args.extend(["--repo".to_owned(), repository.to_owned()]);
        }
        for (key, value) in fields {
            args.extend(["-f".to_owned(), format!("{key}={value}")]);
        }
        self.run_gh(&args).map(|_| ())
    }

    /// Check whether the `gh` CLI is authenticated.
    #[must_use]
    pub fn auth_status(&self) -> bool {
        let Ok(mut command) = self.prepare_gh_command() else {
            return false;
        };
        command.args(["auth", "status"]);
        command.output().is_ok_and(|output| output.status.success())
    }

    /// Poll for the most recent run created by a workflow dispatch.
    pub fn find_dispatched_run(
        &self,
        repository: Option<&str>,
        workflow_file: &str,
        ref_name: &str,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<WorkflowRun, GitHubError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(run) =
                self.latest_workflow_run_for_branch(repository, workflow_file, ref_name)?
            {
                return Ok(run);
            }
            std::thread::sleep(poll_interval);
        }
        Err(GitHubError::new(format!(
            "Workflow run for '{workflow_file}' on '{ref_name}' did not appear within {}s",
            timeout.as_secs()
        )))
    }

    /// Poll for a workflow run created after a known pre-dispatch baseline.
    pub fn find_dispatched_run_after(
        &self,
        repository: Option<&str>,
        workflow_file: &str,
        ref_name: &str,
        previous_database_id: Option<u64>,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<WorkflowRun, GitHubError> {
        if previous_database_id.is_none() {
            return self.find_dispatched_run(
                repository,
                workflow_file,
                ref_name,
                timeout,
                poll_interval,
            );
        }

        let deadline = Instant::now() + timeout;
        let mut last_seen = previous_database_id;
        while Instant::now() < deadline {
            if let Some(run) =
                self.latest_workflow_run_for_branch(repository, workflow_file, ref_name)?
            {
                last_seen = Some(run.database_id);
                if workflow_run_is_newer(&run, previous_database_id) {
                    return Ok(run);
                }
            }
            std::thread::sleep(poll_interval);
        }
        Err(GitHubError::new(format!(
            "New workflow run for '{workflow_file}' on '{ref_name}' did not appear within {}s; baseline={previous_database_id:?} last_seen={last_seen:?}",
            timeout.as_secs()
        )))
    }

    /// Fetch the newest workflow run for `workflow_file` on `branch`.
    pub fn latest_workflow_run_for_branch(
        &self,
        repository: Option<&str>,
        workflow_file: &str,
        branch: &str,
    ) -> Result<Option<WorkflowRun>, GitHubError> {
        let mut args = vec![
            "run".to_owned(),
            "list".to_owned(),
            "--workflow".to_owned(),
            workflow_file.to_owned(),
            "--branch".to_owned(),
            branch.to_owned(),
            "--limit".to_owned(),
            "1".to_owned(),
            "--json".to_owned(),
            RUN_JSON_FIELDS.to_owned(),
        ];
        if let Some(repository) = repository {
            args.extend(["--repo".to_owned(), repository.to_owned()]);
        }
        let stdout = self.run_gh(&args)?;
        parse_first_workflow_run(&stdout)
    }

    /// Fetch the status of one workflow run.
    pub fn workflow_run_status(
        &self,
        repository: Option<&str>,
        run_id: u64,
    ) -> Result<WorkflowRun, GitHubError> {
        let mut args = vec![
            "run".to_owned(),
            "view".to_owned(),
            run_id.to_string(),
            "--json".to_owned(),
            RUN_JSON_FIELDS.to_owned(),
        ];
        if let Some(repository) = repository {
            args.extend(["--repo".to_owned(), repository.to_owned()]);
        }
        let stdout = self.run_gh(&args)?;
        parse_workflow_run(&stdout)
    }

    /// Return active jobs in a run whose names contain `target`.
    pub fn matching_jobs(
        &self,
        repository: &str,
        run_id: u64,
        target: &str,
    ) -> Result<Vec<WorkflowJob>, GitHubError> {
        let args = vec![
            "run".to_owned(),
            "view".to_owned(),
            run_id.to_string(),
            "--repo".to_owned(),
            repository.to_owned(),
            "--json".to_owned(),
            "jobs".to_owned(),
        ];
        let stdout = self.run_gh(&args)?;
        parse_matching_jobs(&stdout, target)
    }

    /// Return every active job in a run.
    pub fn active_jobs(
        &self,
        repository: &str,
        run_id: u64,
    ) -> Result<Vec<WorkflowJob>, GitHubError> {
        let args = vec![
            "run".to_owned(),
            "view".to_owned(),
            run_id.to_string(),
            "--repo".to_owned(),
            repository.to_owned(),
            "--json".to_owned(),
            "jobs".to_owned(),
        ];
        let stdout = self.run_gh(&args)?;
        parse_active_jobs(&stdout)
    }

    /// Cancel a single workflow job.
    pub fn cancel_workflow_job(&self, repository: &str, job_id: u64) -> Result<(), GitHubError> {
        let args = vec![
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{repository}/actions/jobs/{job_id}/cancel"),
        ];
        self.run_gh(&args).map(|_| ())
    }

    /// Cancel a whole workflow run.
    pub fn cancel_workflow_run(&self, repository: &str, run_id: u64) -> Result<(), GitHubError> {
        let args = vec![
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{repository}/actions/runs/{run_id}/cancel"),
        ];
        self.run_gh(&args).map(|_| ())
    }

    /// Re-trigger only the failed jobs in a workflow run. Used by the runner
    /// kill recovery path to immediately re-queue a PR whose Worker we just
    /// terminated.
    pub fn rerun_failed_jobs(&self, repository: &str, run_id: u64) -> Result<(), GitHubError> {
        let args = vec![
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{repository}/actions/runs/{run_id}/rerun-failed-jobs"),
        ];
        self.run_gh(&args).map(|_| ())
    }

    /// Fetch raw status + conclusion for a workflow run as `(status,
    /// conclusion)`. Both fields are returned as raw strings, e.g.
    /// `("completed", "failure")` or `("in_progress", "")`. Used by the runner
    /// kill recovery path to wait for GitHub to recognise that a killed worker
    /// has cleared.
    pub fn run_status_conclusion(
        &self,
        repository: &str,
        run_id: u64,
    ) -> Result<(String, String), GitHubError> {
        let args = vec![
            "api".to_owned(),
            format!("repos/{repository}/actions/runs/{run_id}"),
        ];
        let stdout = self.run_gh(&args)?;
        let value: Value = serde_json::from_str(&stdout)
            .map_err(|error| GitHubError::new(format!("failed to parse run JSON: {error}")))?;
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let conclusion = value
            .get("conclusion")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok((status, conclusion))
    }

    /// List queued workflow runs for a repository.
    pub fn list_queued_runs(
        &self,
        repository: &str,
        limit: u32,
    ) -> Result<Vec<QueuedRun>, GitHubError> {
        let args = vec![
            "api".to_owned(),
            format!("repos/{repository}/actions/runs?status=queued&per_page={limit}"),
        ];
        let stdout = self.run_gh(&args)?;
        parse_queued_runs(&stdout)
    }

    /// Like `list_queued_runs` but paginates up to `max_pages` (`per_page=100` each)
    /// so callers can cover busy repos with more than one page of queued runs.
    /// Stops early when a page returns no items.
    pub fn list_queued_runs_paginated(
        &self,
        repository: &str,
        max_pages: u32,
    ) -> Result<Vec<QueuedRun>, GitHubError> {
        let mut out = Vec::new();
        for page in 1..=max_pages.max(1) {
            let args = vec![
                "api".to_owned(),
                format!("repos/{repository}/actions/runs?status=queued&`per_page=100`&page={page}"),
            ];
            let stdout = self.run_gh(&args)?;
            let page_runs = parse_queued_runs(&stdout)?;
            let count = page_runs.len();
            out.extend(page_runs);
            if count < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// List workflow runs filtered by status (and optional branch).
    pub fn list_runs_with_status(
        &self,
        repository: &str,
        status: &str,
        branch: Option<&str>,
        limit: u32,
    ) -> Result<Vec<QueuedRun>, GitHubError> {
        let mut path = format!("repos/{repository}/actions/runs?status={status}&per_page={limit}");
        if let Some(branch) = branch.filter(|value| !value.is_empty()) {
            path.push_str("&branch=");
            path.push_str(&encode_branch(branch));
        }
        let args = vec!["api".to_owned(), path];
        let stdout = self.run_gh(&args)?;
        parse_queued_runs(&stdout)
    }

    /// Like `list_runs_with_status` but paginates up to `max_pages` (`per_page=100`
    /// each). Stops early when a page returns no items.
    pub fn list_runs_with_status_paginated(
        &self,
        repository: &str,
        status: &str,
        branch: Option<&str>,
        max_pages: u32,
    ) -> Result<Vec<QueuedRun>, GitHubError> {
        let mut out = Vec::new();
        for page in 1..=max_pages.max(1) {
            let mut path = format!(
                "repos/{repository}/actions/runs?status={status}&`per_page=100`&page={page}"
            );
            if let Some(branch) = branch.filter(|value| !value.is_empty()) {
                path.push_str("&branch=");
                path.push_str(&encode_branch(branch));
            }
            let args = vec!["api".to_owned(), path];
            let stdout = self.run_gh(&args)?;
            let page_runs = parse_queued_runs(&stdout)?;
            let count = page_runs.len();
            out.extend(page_runs);
            if count < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// Re-arm the failed jobs of a workflow run (`gh run rerun --failed`).
    pub fn rerun_failed_run(&self, repository: &str, run_id: u64) -> Result<(), GitHubError> {
        let args = vec![
            "run".to_owned(),
            "rerun".to_owned(),
            run_id.to_string(),
            "--failed".to_owned(),
            "--repo".to_owned(),
            repository.to_owned(),
        ];
        self.run_gh(&args).map(|_| ())
    }

    /// Fetch metadata for a workflow run.
    pub fn run_metadata(&self, repository: &str, run_id: u64) -> Result<RunMetadata, GitHubError> {
        let args = vec![
            "api".to_owned(),
            format!("repos/{repository}/actions/runs/{run_id}"),
        ];
        let stdout = self.run_gh(&args)?;
        parse_run_metadata(&stdout)
    }

    /// Fetch a PR's head ref from GitHub.
    pub fn pr_head_ref(&self, repository: &str, pr: u64) -> Result<Option<String>, GitHubError> {
        let args = vec![
            "pr".to_owned(),
            "view".to_owned(),
            pr.to_string(),
            "--repo".to_owned(),
            repository.to_owned(),
            "--json".to_owned(),
            "headRefName,number,state".to_owned(),
        ];
        let stdout = match self.run_gh(&args) {
            Ok(stdout) => stdout,
            Err(error) if crate::gh::is_graphql_rate_limited(error.message()) => {
                return self.pr_head_ref_rest(repository, pr);
            }
            Err(error) => return Err(error),
        };
        let value = serde_json::from_str::<Value>(&stdout).map_err(|error| {
            GitHubError::new(format!("failed to parse gh pr view JSON: {error}"))
        })?;
        Ok(value
            .get("headRefName")
            .and_then(Value::as_str)
            .filter(|head| !head.is_empty())
            .map(ToOwned::to_owned))
    }

    fn pr_head_ref_rest(&self, repository: &str, pr: u64) -> Result<Option<String>, GitHubError> {
        let args = vec!["api".to_owned(), format!("repos/{repository}/pulls/{pr}")];
        let stdout = self.run_gh(&args).map_err(|error| {
            GitHubError::new(format!(
                "gh pr view hit GraphQL rate limit, then REST PR lookup failed: {error}"
            ))
        })?;
        let value = serde_json::from_str::<Value>(&stdout)
            .map_err(|error| GitHubError::new(format!("failed to parse REST PR JSON: {error}")))?;
        Ok(value
            .get("head")
            .and_then(|head| head.get("ref"))
            .and_then(Value::as_str)
            .filter(|head| !head.is_empty())
            .map(ToOwned::to_owned))
    }

    pub(crate) fn run_gh(&self, args: &[String]) -> Result<String, GitHubError> {
        let output = self
            .prepare_gh_command()?
            .args(args)
            .output()
            .map_err(|error| {
                GitHubError::new(format!("failed to run gh {}: {error}", args.join(" ")))
            })?;
        if !output.status.success() {
            return Err(GitHubError::command_failed(
                args,
                output.status.code(),
                &output.stderr,
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn prepare_gh_command(&self) -> Result<std::process::Command, GitHubError> {
        let client = self
            .gh
            .as_ref()
            .map_err(|error| GitHubError::new(error.clone()))?;
        client
            .prepare_command(
                &self.cwd,
                self.gh_binary_override.as_deref(),
                GhSupervision::Unsupervised,
                GhAuthPolicy::Default,
            )
            .map_err(|error| GitHubError::new(format!("failed to prepare gh command: {error}")))
    }
}

/// Discover workflow-dispatchable GitHub Actions workflows below `repo_root`.
#[must_use]
pub fn discover_workflows(repo_root: &Path) -> BTreeMap<String, WorkflowDefinition> {
    let workflow_dir = repo_root.join(".github").join("workflows");
    let mut paths = Vec::new();
    let Ok(entries) = fs::read_dir(workflow_dir) else {
        return BTreeMap::new();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        if matches!(extension, "yml" | "yaml") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut discovered = BTreeMap::new();
    for path in paths {
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(filename) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let contents = fs::read_to_string(&path).unwrap_or_default();
        let inputs = discover_workflow_inputs(&contents);
        let key = stem.to_owned();
        let name = discover_workflow_name(&contents).unwrap_or_else(|| titleize(stem));
        let definition = WorkflowDefinition {
            key: key.clone(),
            file: filename.to_owned(),
            description: format!("{name} ({filename})"),
            name,
            inputs,
        };
        discovered.insert(key.clone(), definition.clone());
        if key == "ci" && !discovered.contains_key("build") {
            discovered.insert(
                "build".to_owned(),
                WorkflowDefinition {
                    key: "build".to_owned(),
                    ..definition
                },
            );
        }
    }
    discovered
}

/// Resolve the default cloud workflow key.
#[must_use]
pub fn default_workflow_key(
    config: &LoadedConfig,
    workflows: &BTreeMap<String, WorkflowDefinition>,
) -> Option<String> {
    if let Some(configured) = config.get_str("cloud.default_workflow")
        && workflows.contains_key(configured)
    {
        return Some(configured.to_owned());
    }
    if workflows.contains_key("build") {
        return Some("build".to_owned());
    }
    workflows.keys().next().cloned()
}

/// Resolve provider and dispatch fields for a workflow.
pub fn resolve_cloud_dispatch_plan(
    config: &LoadedConfig,
    workflows: &BTreeMap<String, WorkflowDefinition>,
    workflow_key: &str,
    ref_name: &str,
    provider_override: Option<&str>,
) -> Result<CloudDispatchPlan, String> {
    resolve_cloud_dispatch_plan_with_overrides(
        config,
        workflows,
        workflow_key,
        ref_name,
        CloudDispatchOverrides::provider(provider_override),
    )
}

/// Resolve a dispatch plan while layering CLI overrides above config/provider defaults.
pub fn resolve_cloud_dispatch_plan_with_overrides(
    config: &LoadedConfig,
    workflows: &BTreeMap<String, WorkflowDefinition>,
    workflow_key: &str,
    ref_name: &str,
    overrides: CloudDispatchOverrides<'_>,
) -> Result<CloudDispatchPlan, String> {
    let workflow = workflows
        .get(workflow_key)
        .cloned()
        .ok_or_else(|| format!("Unknown workflow '{workflow_key}'"))?;
    let repository = config.get_str("cloud.repository").map(ToOwned::to_owned);
    let workflow_config_key = format!("cloud.workflows.{workflow_key}");
    let workflow_config = table_at(config, &workflow_config_key);

    let mut sources = BTreeMap::new();
    let provider = if let Some(provider) = overrides.provider.filter(|value| !value.is_empty()) {
        sources.insert("provider".to_owned(), "cli".to_owned());
        provider.to_owned()
    } else if let Some(provider) = config.get_str(&format!("{workflow_config_key}.provider")) {
        sources.insert(
            "provider".to_owned(),
            format!("config:cloud.workflows.{workflow_key}.provider"),
        );
        provider.to_owned()
    } else {
        let provider = config
            .get_str("cloud.provider")
            .unwrap_or("github-hosted")
            .to_owned();
        sources.insert(
            "provider".to_owned(),
            if config.get_str("cloud.provider").is_some() {
                "config:cloud.provider".to_owned()
            } else {
                "default".to_owned()
            },
        );
        provider
    };

    let provider_config = table_at(config, &format!("cloud.providers.{provider}"));
    let mut dispatch_fields = BTreeMap::new();
    let inputs: BTreeSet<&str> = workflow.inputs.iter().map(String::as_str).collect();

    if inputs.contains("runner_provider") {
        dispatch_fields.insert("runner_provider".to_owned(), provider.clone());
    }

    if let Some(selector) = overrides
        .runner_selector
        .filter(|selector| inputs.contains("runner_selector") && !selector.is_empty())
    {
        dispatch_fields.insert("runner_selector".to_owned(), selector.to_owned());
        sources.insert("runner_selector".to_owned(), "cli".to_owned());
    } else if inputs.contains("runner_selector")
        && let Some((selector, source)) = workflow_config
            .and_then(|table| table_str(table, "runner_selector"))
            .map(|selector| {
                (
                    selector,
                    format!("config:cloud.workflows.{workflow_key}.runner_selector"),
                )
            })
            .or_else(|| {
                provider_config
                    .and_then(|table| table_str(table, "runner_selector"))
                    .map(|selector| {
                        (
                            selector,
                            format!("config:cloud.providers.{provider}.runner_selector"),
                        )
                    })
            })
    {
        dispatch_fields.insert("runner_selector".to_owned(), selector.to_owned());
        sources.insert("runner_selector".to_owned(), source);
    }

    let cli_platform_selectors = overrides.platform_selectors();
    let runner_overrides = resolve_runner_overrides(
        &provider,
        provider_config,
        workflow_config,
        &cli_platform_selectors,
    );
    if !runner_overrides.is_empty() && inputs.contains("runner_overrides") {
        let encoded = serde_json::to_string(&runner_overrides)
            .map_err(|error| format!("failed to encode runner_overrides: {error}"))?;
        dispatch_fields.insert("runner_overrides".to_owned(), encoded);
        sources.insert(
            "runner_overrides".to_owned(),
            "cli/config/provider".to_owned(),
        );
    } else {
        apply_platform_specific_inputs(&inputs, &runner_overrides, &mut dispatch_fields);
    }

    Ok(CloudDispatchPlan {
        workflow,
        repository,
        ref_name: ref_name.to_owned(),
        provider,
        dispatch_fields,
        sources,
    })
}

/// Return whether a lane is merge-blocking according to config.
#[must_use]
pub fn lane_is_required(config: &LoadedConfig, target: &str) -> bool {
    !config
        .get(&format!("targets.{target}.advisory"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false)
}

/// Parse the first workflow run from `gh run list --json ...`.
pub fn parse_first_workflow_run(stdout: &str) -> Result<Option<WorkflowRun>, GitHubError> {
    let runs = serde_json::from_str::<Vec<Value>>(stdout)
        .map_err(|error| GitHubError::new(format!("failed to parse gh run list JSON: {error}")))?;
    let Some(first) = runs.first() else {
        return Ok(None);
    };
    workflow_run_from_value(first).map(Some)
}

/// Parse one workflow run from `gh run view --json ...`.
pub fn parse_workflow_run(stdout: &str) -> Result<WorkflowRun, GitHubError> {
    let value = serde_json::from_str::<Value>(stdout)
        .map_err(|error| GitHubError::new(format!("failed to parse gh run view JSON: {error}")))?;
    workflow_run_from_value(&value)
}

/// Parse active jobs whose names match `target`.
pub fn parse_matching_jobs(stdout: &str, target: &str) -> Result<Vec<WorkflowJob>, GitHubError> {
    let value = serde_json::from_str::<Value>(stdout)
        .map_err(|error| GitHubError::new(format!("failed to parse gh run view JSON: {error}")))?;
    let Some(jobs) = value.get("jobs").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let needle = target.to_lowercase();
    let mut matches = Vec::new();
    for job in jobs {
        let Some(name) = job.get("name").and_then(Value::as_str) else {
            continue;
        };
        let status = job
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !JOB_ACTIVE_STATUSES.contains(&status) || !name.to_lowercase().contains(&needle) {
            continue;
        }
        let Some(database_id) = job.get("databaseId").and_then(Value::as_u64) else {
            continue;
        };
        matches.push(WorkflowJob {
            database_id,
            name: name.to_owned(),
        });
    }
    Ok(matches)
}

/// Parse all active jobs in a workflow run.
pub fn parse_active_jobs(stdout: &str) -> Result<Vec<WorkflowJob>, GitHubError> {
    let value = serde_json::from_str::<Value>(stdout)
        .map_err(|error| GitHubError::new(format!("failed to parse gh run view JSON: {error}")))?;
    let Some(jobs) = value.get("jobs").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut active = Vec::new();
    for job in jobs {
        let status = job
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !JOB_ACTIVE_STATUSES.contains(&status) {
            continue;
        }
        let Some(database_id) = job.get("databaseId").and_then(Value::as_u64) else {
            continue;
        };
        let name = job
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        active.push(WorkflowJob { database_id, name });
    }
    Ok(active)
}

/// Parse queued workflow runs from `gh api repos/:repo/actions/runs`.
pub fn parse_queued_runs(stdout: &str) -> Result<Vec<QueuedRun>, GitHubError> {
    let value = serde_json::from_str::<Value>(stdout)
        .map_err(|error| GitHubError::new(format!("failed to parse gh runs JSON: {error}")))?;
    let Some(runs) = value.get("workflow_runs").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for run in runs {
        let Some(database_id) = run.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let name = run
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let head_branch = run
            .get("head_branch")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let created_at = run
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let path = run
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let status = run
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let conclusion = run
            .get("conclusion")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        out.push(QueuedRun {
            database_id,
            workflow_name: name.clone(),
            name,
            head_branch,
            created_at,
            url: run
                .get("html_url")
                .or_else(|| run.get("url"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            path,
            status,
            conclusion,
        });
    }
    Ok(out)
}

/// Parse workflow run metadata from the GitHub Actions API.
pub fn parse_run_metadata(stdout: &str) -> Result<RunMetadata, GitHubError> {
    let value = serde_json::from_str::<Value>(stdout)
        .map_err(|error| GitHubError::new(format!("failed to parse gh run JSON: {error}")))?;
    Ok(RunMetadata {
        path: value
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        head_branch: value
            .get("head_branch")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        status: value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn encode_branch(branch: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(branch.len());
    for ch in branch.chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                let encoded = ch.encode_utf8(&mut buf);
                for byte in encoded.bytes() {
                    write!(out, "%{byte:02X}").expect("write to String");
                }
            }
        }
    }
    out
}

fn workflow_run_from_value(value: &Value) -> Result<WorkflowRun, GitHubError> {
    let database_id = value
        .get("databaseId")
        .and_then(Value::as_u64)
        .ok_or_else(|| GitHubError::new("gh run JSON missing databaseId"))?;
    Ok(WorkflowRun {
        database_id,
        status: value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        conclusion: value
            .get("conclusion")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        url: value
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

pub(crate) fn workflow_run_is_newer(run: &WorkflowRun, previous_database_id: Option<u64>) -> bool {
    previous_database_id.is_none_or(|previous| run.database_id > previous)
}

fn discover_workflow_name(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let stripped = line.trim();
        let rest = stripped.strip_prefix("name:")?;
        Some(rest.trim().trim_matches(['"', '\'']).to_owned())
    })
}

fn discover_workflow_inputs(contents: &str) -> Vec<String> {
    let mut inputs = Vec::new();
    let mut in_workflow_dispatch = false;
    let mut in_inputs = false;
    let mut workflow_indent = None;
    let mut inputs_indent = None;

    for raw_line in contents.lines() {
        if raw_line.trim().is_empty() || raw_line.trim_start().starts_with('#') {
            continue;
        }
        let indent = raw_line.len() - raw_line.trim_start_matches(' ').len();
        let stripped = raw_line.trim();

        if stripped.starts_with("workflow_dispatch:") {
            in_workflow_dispatch = true;
            in_inputs = false;
            workflow_indent = Some(indent);
            continue;
        }
        if in_workflow_dispatch
            && workflow_indent.is_some_and(|workflow_indent| indent <= workflow_indent)
            && stripped.contains(':')
        {
            in_workflow_dispatch = false;
            in_inputs = false;
        }
        if in_workflow_dispatch && stripped.starts_with("inputs:") {
            in_inputs = true;
            inputs_indent = Some(indent);
            continue;
        }
        if in_inputs && inputs_indent.is_some_and(|inputs_indent| indent <= inputs_indent) {
            in_inputs = false;
        }
        if in_inputs
            && inputs_indent.is_some_and(|inputs_indent| indent == inputs_indent + 2)
            && stripped.ends_with(':')
        {
            inputs.push(stripped.trim_end_matches(':').to_owned());
        }
    }
    inputs
}

fn titleize(key: &str) -> String {
    key.replace(['-', '_'], " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn table_at<'a>(config: &'a LoadedConfig, dotted_key: &str) -> Option<&'a Table> {
    config.get(dotted_key)?.as_table()
}

fn table_str<'a>(table: &'a Table, key: &str) -> Option<&'a str> {
    table.get(key)?.as_str()
}

fn resolve_runner_overrides(
    provider: &str,
    provider_config: Option<&Table>,
    workflow_config: Option<&Table>,
    cli_overrides: &BTreeMap<&str, &str>,
) -> BTreeMap<String, String> {
    let mut overrides = BTreeMap::new();
    for platform in ["linux-x64", "windows-x64", "macos-arm64"] {
        if let Some(value) = cli_overrides
            .get(platform)
            .filter(|value| !value.is_empty())
            .map(|value| (*value).to_owned())
            .or_else(|| nested_table_str(workflow_config, "runner_overrides", platform))
            .or_else(|| nested_table_str(provider_config, "runner_overrides", platform))
            .or_else(|| resolve_provider_selector(provider, platform, provider_config))
        {
            overrides.insert(platform.to_owned(), value);
        }
    }
    overrides
}

fn nested_table_str(table: Option<&Table>, section: &str, key: &str) -> Option<String> {
    table?
        .get(section)?
        .as_table()?
        .get(key)?
        .as_str()
        .map(ToOwned::to_owned)
}

fn resolve_provider_selector(
    provider: &str,
    platform: &str,
    provider_config: Option<&Table>,
) -> Option<String> {
    match provider {
        "github-hosted" => github_hosted_selector(platform),
        "namespace" => namespace_selector(platform, provider_config),
        _ => None,
    }
}

fn github_hosted_selector(platform: &str) -> Option<String> {
    match platform {
        "linux-x64" | "linux" | "ubuntu" => Some("ubuntu-latest".to_owned()),
        "windows-x64" | "windows" => Some("windows-latest".to_owned()),
        "macos-arm64" | "macos" => Some("macos-15".to_owned()),
        "macos-x64" => Some("macos-13".to_owned()),
        _ => None,
    }
}

fn namespace_selector(platform: &str, provider_config: Option<&Table>) -> Option<String> {
    if let Some(value) = nested_table_str(provider_config, "profiles", platform) {
        return Some(format!("namespace-profile-{value}"));
    }
    let machine = nested_table_str(provider_config, "machines", platform)?;
    if machine.starts_with("nscloud-") {
        Some(machine)
    } else {
        Some(format!("nscloud-{machine}"))
    }
}

fn apply_platform_specific_inputs(
    inputs: &BTreeSet<&str>,
    overrides: &BTreeMap<String, String>,
    dispatch_fields: &mut BTreeMap<String, String>,
) {
    for (platform, input) in [
        ("linux-x64", "linux_runner_selector"),
        ("windows-x64", "windows_runner_selector"),
        ("macos-arm64", "macos_runner_selector"),
    ] {
        if inputs.contains(input)
            && let Some(value) = overrides.get(platform)
        {
            dispatch_fields.insert(input.to_owned(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::path::Path;

    use tempfile::TempDir;

    use super::{
        CloudDispatchOverrides, default_workflow_key, discover_workflows, parse_active_jobs,
        parse_first_workflow_run, parse_matching_jobs, resolve_cloud_dispatch_plan,
        resolve_cloud_dispatch_plan_with_overrides,
    };
    use crate::config::{LoadedConfig, LocalOverlaySource};

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).expect("write script");
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod script");
    }

    #[cfg(unix)]
    fn config_with_command_helper(helper: &Path) -> LoadedConfig {
        let text = format!(
            r#"
[github.auth]
source = "command"
token_command = ["{}"]
cache_ttl_seconds = 300
"#,
            helper.display()
        );
        LoadedConfig {
            data: text.parse().expect("parse config"),
            global_dir: Path::new("/tmp/shipyard-global").to_path_buf(),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn pr_head_ref_falls_back_to_rest_with_configured_app_token() {
        let temp = TempDir::new().expect("tempdir");
        let helper = temp.path().join("token-helper");
        write_executable(
            &helper,
            r#"#!/bin/sh
printf '{"token":"ghs_app_token","kind":"github-app-installation","expires_at":"2099-01-01T00:00:00Z"}'
"#,
        );

        let log = temp.path().join("gh.log");
        let gh = temp.path().join("gh");
        write_executable(
            &gh,
            &format!(
                r#"#!/bin/sh
printf 'GH_TOKEN=%s ARGS=%s\n' "$GH_TOKEN" "$*" >> '{}'
case "$1 $2" in
  "pr view")
    printf 'GraphQL: API rate limit already exceeded\n' >&2
    exit 1
    ;;
  "api repos/danielraffel/pulp/pulls/314")
    printf '{{"head":{{"ref":"feat/app-quota"}}}}'
    exit 0
    ;;
esac
printf 'unexpected gh args: %s\n' "$*" >&2
exit 2
"#,
                log.display()
            ),
        );

        let config = config_with_command_helper(&helper);
        let client = super::GitHubActions::from_loaded_config(temp.path(), &config)
            .with_gh_binary_for_tests(&gh);

        let head = client
            .pr_head_ref("danielraffel/pulp", 314)
            .expect("head ref");

        assert_eq!(head.as_deref(), Some("feat/app-quota"));
        let log = std::fs::read_to_string(log).expect("gh log");
        assert!(log.contains("GH_TOKEN=ghs_app_token ARGS=pr view 314"));
        assert!(log.contains("GH_TOKEN=ghs_app_token ARGS=api repos/danielraffel/pulp/pulls/314"));
    }

    #[test]
    fn discovers_workflow_dispatch_inputs_and_build_alias() {
        let temp = TempDir::new().expect("tempdir");
        let workflows = temp.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).expect("workflows dir");
        std::fs::write(
            workflows.join("ci.yml"),
            r"
name: CI
on:
  workflow_dispatch:
    inputs:
      runner_provider:
        required: false
      macos_runner_selector:
        required: false
",
        )
        .expect("workflow");

        let discovered = discover_workflows(temp.path());

        assert!(discovered.contains_key("ci"));
        assert_eq!(discovered["build"].file, "ci.yml");
        assert_eq!(
            discovered["build"].inputs,
            vec!["runner_provider", "macos_runner_selector"]
        );
    }

    #[test]
    fn resolves_provider_and_platform_specific_inputs() {
        let temp = TempDir::new().expect("tempdir");
        let project = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(
            project.join("config.toml"),
            r#"
[cloud]
provider = "namespace"

[cloud.providers.namespace.profiles]
macos-arm64 = "generouscorp-macos"
"#,
        )
        .expect("config");
        let config = LoadedConfig::load(None, Some(project), None, LocalOverlaySource::None)
            .expect("config");
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "build".to_owned(),
            super::WorkflowDefinition {
                key: "build".to_owned(),
                file: "ci.yml".to_owned(),
                name: "CI".to_owned(),
                description: "CI".to_owned(),
                inputs: vec![
                    "runner_provider".to_owned(),
                    "macos_runner_selector".to_owned(),
                ],
            },
        );

        let plan =
            resolve_cloud_dispatch_plan(&config, &workflows, "build", "feature/x", None).unwrap();

        assert_eq!(
            default_workflow_key(&config, &workflows),
            Some("build".to_owned())
        );
        assert_eq!(plan.provider, "namespace");
        assert_eq!(plan.dispatch_fields["runner_provider"], "namespace");
        assert_eq!(
            plan.dispatch_fields["macos_runner_selector"],
            "namespace-profile-generouscorp-macos"
        );
    }

    #[test]
    fn cli_selector_overrides_win_over_config_and_provider_defaults() {
        let temp = TempDir::new().expect("tempdir");
        let project = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(
            project.join("config.toml"),
            r#"
[cloud]
provider = "github-hosted"

[cloud.providers.github-hosted]
runner_selector = "config-selector"
"#,
        )
        .expect("config");
        let config = LoadedConfig::load(None, Some(project), None, LocalOverlaySource::None)
            .expect("config");
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "build".to_owned(),
            super::WorkflowDefinition {
                key: "build".to_owned(),
                file: "ci.yml".to_owned(),
                name: "CI".to_owned(),
                description: "CI".to_owned(),
                inputs: vec![
                    "runner_provider".to_owned(),
                    "runner_selector".to_owned(),
                    "runner_overrides".to_owned(),
                ],
            },
        );

        let plan = resolve_cloud_dispatch_plan_with_overrides(
            &config,
            &workflows,
            "build",
            "feature/x",
            CloudDispatchOverrides {
                provider: None,
                runner_selector: Some("cli-selector"),
                linux_runner_selector: Some("custom-linux"),
                windows_runner_selector: None,
                macos_runner_selector: None,
            },
        )
        .unwrap();

        assert_eq!(plan.dispatch_fields["runner_provider"], "github-hosted");
        assert_eq!(plan.dispatch_fields["runner_selector"], "cli-selector");
        assert_eq!(plan.sources["runner_selector"], "cli");
        let runner_overrides: BTreeMap<String, String> =
            serde_json::from_str(&plan.dispatch_fields["runner_overrides"]).unwrap();
        assert_eq!(runner_overrides["linux-x64"], "custom-linux");
        assert_eq!(runner_overrides["windows-x64"], "windows-latest");
        assert_eq!(runner_overrides["macos-arm64"], "macos-15");
    }

    #[test]
    fn parses_first_workflow_run() {
        let parsed = parse_first_workflow_run(
            r#"[{"databaseId":123,"status":"queued","conclusion":null,"url":"https://example"}]"#,
        )
        .expect("parse")
        .expect("run");

        assert_eq!(parsed.database_id, 123);
        assert_eq!(parsed.status, "queued");
        assert_eq!(parsed.url.as_deref(), Some("https://example"));
    }

    #[test]
    fn workflow_run_newer_requires_id_after_baseline() {
        let run = super::WorkflowRun {
            database_id: 200,
            status: "queued".to_owned(),
            conclusion: None,
            url: None,
        };

        assert!(super::workflow_run_is_newer(&run, None));
        assert!(super::workflow_run_is_newer(&run, Some(199)));
        assert!(!super::workflow_run_is_newer(&run, Some(200)));
        assert!(!super::workflow_run_is_newer(&run, Some(201)));
    }

    #[test]
    fn matching_jobs_filters_active_case_insensitive_substrings() {
        let parsed = parse_matching_jobs(
            r#"{"jobs":[
                {"databaseId":1,"name":"macOS (ARM64) [namespace]","status":"in_progress"},
                {"databaseId":2,"name":"macOS old","status":"completed"},
                {"databaseId":3,"name":"Linux","status":"queued"}
            ]}"#,
            "macos",
        )
        .expect("parse");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].database_id, 1);
    }

    #[test]
    fn active_jobs_filters_terminal_jobs() {
        let parsed = parse_active_jobs(
            r#"{"jobs":[
                {"databaseId":1,"name":"Cloud Live Smoke","status":"in_progress"},
                {"databaseId":2,"name":"Linux","status":"queued"},
                {"databaseId":3,"name":"old","status":"completed"}
            ]}"#,
        )
        .expect("parse");

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].database_id, 1);
        assert_eq!(parsed[1].database_id, 2);
    }

    #[test]
    fn queued_runs_parse_github_api_shape() {
        let parsed = super::parse_queued_runs(
            r#"{"workflow_runs":[{"id":555,"name":"CI","head_branch":"feat/x","created_at":"2026-04-23T12:00:00Z","html_url":"https://example/run/555","path":".github/workflows/ci.yml"}]}"#,
        )
        .expect("parse");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].database_id, 555);
        assert_eq!(parsed[0].path, ".github/workflows/ci.yml");
    }

    #[test]
    fn run_metadata_parses_workflow_file_and_ref() {
        let parsed = super::parse_run_metadata(
            r#"{"path":".github/workflows/ci.yml","head_branch":"feat/x","name":"CI","status":"queued"}"#,
        )
        .expect("parse");

        assert_eq!(parsed.path, ".github/workflows/ci.yml");
        assert_eq!(parsed.head_branch, "feat/x");
    }
}
