//! GitHub Actions cloud executor.
//!
//! This backend mirrors Python Shipyard's cloud executor: dispatch a
//! workflow, wait for the run to appear, then poll it to completion.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::classify::FailureClass;
use crate::cloud::{GitHubActions, GitHubError, WorkflowRun, workflow_run_is_newer};
use crate::executor::streaming::ProgressEvent;
use crate::job::{TargetResult, TargetStatus};

const DEFAULT_POLL_INTERVAL_SECS: u64 = 15;
const DEFAULT_DISPATCH_SETTLE_SECS: u64 = 30;
const DEFAULT_MAX_POLL_SECS: u64 = 3_600;

/// Side-effect boundary for GitHub Actions operations.
pub trait CloudActionsClient {
    /// Return whether `gh auth status` succeeds.
    fn auth_status(&self) -> bool;

    /// Dispatch a workflow run.
    fn workflow_dispatch(
        &self,
        repository: Option<&str>,
        workflow_file: &str,
        ref_name: &str,
        fields: &BTreeMap<String, String>,
    ) -> Result<(), GitHubError>;

    /// Return the newest run for a workflow/branch pair.
    fn latest_workflow_run_for_branch(
        &self,
        repository: Option<&str>,
        workflow_file: &str,
        branch: &str,
    ) -> Result<Option<WorkflowRun>, GitHubError>;

    /// Return the current status of a run.
    fn workflow_run_status(
        &self,
        repository: Option<&str>,
        run_id: u64,
    ) -> Result<WorkflowRun, GitHubError>;
}

impl CloudActionsClient for GitHubActions {
    fn auth_status(&self) -> bool {
        GitHubActions::auth_status(self)
    }

    fn workflow_dispatch(
        &self,
        repository: Option<&str>,
        workflow_file: &str,
        ref_name: &str,
        fields: &BTreeMap<String, String>,
    ) -> Result<(), GitHubError> {
        GitHubActions::workflow_dispatch(self, repository, workflow_file, ref_name, fields)
    }

    fn latest_workflow_run_for_branch(
        &self,
        repository: Option<&str>,
        workflow_file: &str,
        branch: &str,
    ) -> Result<Option<WorkflowRun>, GitHubError> {
        GitHubActions::latest_workflow_run_for_branch(self, repository, workflow_file, branch)
    }

    fn workflow_run_status(
        &self,
        repository: Option<&str>,
        run_id: u64,
    ) -> Result<WorkflowRun, GitHubError> {
        GitHubActions::workflow_run_status(self, repository, run_id)
    }
}

/// Cloud target configuration resolved from Shipyard config.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudTargetConfig {
    /// Target name.
    pub name: String,
    /// Platform label.
    pub platform: String,
    /// Workflow file or name accepted by `gh workflow run`.
    pub workflow: String,
    /// Optional repository slug.
    pub repository: Option<String>,
    /// Runner provider, e.g. `namespace`.
    pub runner_provider: Option<String>,
    /// Optional runner selector/profile.
    pub runner_selector: Option<String>,
    /// Optional runner override map encoded into workflow inputs.
    pub runner_overrides: BTreeMap<String, String>,
    /// Poll cadence while waiting for the run to appear and finish.
    pub poll_interval_secs: u64,
    /// Maximum wait for the dispatched run to appear.
    pub dispatch_settle_secs: u64,
    /// Maximum wait for the workflow run to complete.
    pub max_poll_secs: u64,
    /// Optional per-target failure parser name (issue #303).
    pub failure_parser: Option<String>,
}

impl CloudTargetConfig {
    /// Construct a config with Python-compatible timing defaults.
    #[must_use]
    pub fn new(name: impl Into<String>, platform: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            platform: platform.into(),
            workflow: "ci.yml".to_owned(),
            repository: None,
            runner_provider: None,
            runner_selector: None,
            runner_overrides: BTreeMap::new(),
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            dispatch_settle_secs: DEFAULT_DISPATCH_SETTLE_SECS,
            max_poll_secs: DEFAULT_MAX_POLL_SECS,
            failure_parser: None,
        }
    }
}

/// Cloud validation request.
pub struct CloudValidationRequest<'a> {
    /// Commit SHA under validation.
    pub sha: String,
    /// Branch under validation.
    pub branch: String,
    /// Target config.
    pub target: CloudTargetConfig,
    /// Local log path used for result parity.
    pub log_path: PathBuf,
    /// Optional progress callback.
    pub progress_callback:
        Option<&'a mut dyn FnMut(ProgressEvent) -> crate::executor::streaming::ProgressAction>,
}

impl CloudValidationRequest<'_> {
    /// Build a request with the required fields.
    #[must_use]
    pub fn new(log_path: PathBuf, target: CloudTargetConfig) -> Self {
        Self {
            sha: String::new(),
            branch: String::new(),
            target,
            log_path,
            progress_callback: None,
        }
    }
}

/// GitHub Actions-backed cloud executor.
#[derive(Clone, Debug)]
pub struct CloudExecutor<C = GitHubActions> {
    client: C,
}

impl CloudExecutor<GitHubActions> {
    /// Construct a production cloud executor.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            client: GitHubActions::new(cwd),
        }
    }
}

impl<C: CloudActionsClient> CloudExecutor<C> {
    /// Construct an executor around a test or production client.
    #[must_use]
    pub fn with_client(client: C) -> Self {
        Self { client }
    }

    /// Return whether GitHub Actions is reachable enough to submit work.
    #[must_use]
    pub fn probe(&self) -> bool {
        self.client.auth_status()
    }

    /// Validate a target through GitHub Actions.
    #[must_use]
    pub fn validate(&self, mut request: CloudValidationRequest<'_>) -> TargetResult {
        let started_at = Utc::now();
        let started = Instant::now();
        let fields = dispatch_fields(&request.target);
        let baseline_database_id = match self.client.latest_workflow_run_for_branch(
            request.target.repository.as_deref(),
            &request.target.workflow,
            &request.branch,
        ) {
            Ok(run) => run.map(|run| run.database_id),
            Err(error) => {
                return terminal_result(
                    &request.target,
                    started_at,
                    started,
                    &request.log_path,
                    TargetStatus::Error,
                    Some(format!("Failed to inspect existing workflow run: {error}")),
                    Some(FailureClass::Infra),
                );
            }
        };

        if let Err(error) = self.client.workflow_dispatch(
            request.target.repository.as_deref(),
            &request.target.workflow,
            &request.branch,
            &fields,
        ) {
            return terminal_result(
                &request.target,
                started_at,
                started,
                &request.log_path,
                TargetStatus::Error,
                Some(format!("Failed to dispatch workflow: {error}")),
                Some(FailureClass::Infra),
            );
        }
        emit_progress(&mut request.progress_callback, "dispatch");

        let run = match self.wait_for_run(&request, baseline_database_id, started_at, started) {
            Ok(run) => run,
            Err(result) => return *result,
        };
        emit_progress(&mut request.progress_callback, "queued");

        self.poll_run(request, run.database_id, started_at, started)
    }

    fn wait_for_run(
        &self,
        request: &CloudValidationRequest<'_>,
        baseline_database_id: Option<u64>,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> Result<WorkflowRun, Box<TargetResult>> {
        let deadline = Instant::now() + Duration::from_secs(request.target.dispatch_settle_secs);
        let mut last_seen = baseline_database_id;
        loop {
            match self.client.latest_workflow_run_for_branch(
                request.target.repository.as_deref(),
                &request.target.workflow,
                &request.branch,
            ) {
                Ok(Some(run)) => {
                    last_seen = Some(run.database_id);
                    if workflow_run_is_newer(&run, baseline_database_id) {
                        return Ok(run);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(Box::new(terminal_result(
                        &request.target,
                        started_at,
                        started,
                        &request.log_path,
                        TargetStatus::Error,
                        Some(format!("Failed to find workflow run: {error}")),
                        Some(FailureClass::Infra),
                    )));
                }
            }
            if Instant::now() >= deadline {
                let message = if baseline_database_id.is_some() {
                    format!(
                        "New workflow run did not appear within {}s; baseline={baseline_database_id:?} last_seen={last_seen:?}",
                        request.target.dispatch_settle_secs
                    )
                } else {
                    format!(
                        "Workflow run did not appear within {}s",
                        request.target.dispatch_settle_secs
                    )
                };
                return Err(Box::new(terminal_result(
                    &request.target,
                    started_at,
                    started,
                    &request.log_path,
                    TargetStatus::Error,
                    Some(message),
                    Some(FailureClass::Timeout),
                )));
            }
            sleep_poll(request.target.poll_interval_secs, 5);
        }
    }

    fn poll_run(
        &self,
        mut request: CloudValidationRequest<'_>,
        run_id: u64,
        started_at: DateTime<Utc>,
        started: Instant,
    ) -> TargetResult {
        let deadline = Instant::now() + Duration::from_secs(request.target.max_poll_secs);
        loop {
            match self
                .client
                .workflow_run_status(request.target.repository.as_deref(), run_id)
            {
                Ok(run) => {
                    let phase = if run.status.is_empty() {
                        "poll"
                    } else {
                        &run.status
                    };
                    emit_progress(&mut request.progress_callback, phase);
                    if run.status == "completed" {
                        let passed = run.conclusion.as_deref() == Some("success");
                        let cancelled =
                            matches!(run.conclusion.as_deref(), Some("cancelled" | "skipped"));
                        let status = if passed {
                            TargetStatus::Pass
                        } else if cancelled {
                            TargetStatus::Cancelled
                        } else {
                            TargetStatus::Fail
                        };
                        let failure_class = (!passed && !cancelled).then_some(FailureClass::Test);
                        let mut result = terminal_result(
                            &request.target,
                            started_at,
                            started,
                            &request.log_path,
                            status,
                            None,
                            failure_class,
                        );
                        // Persist the GHA run_id so downstream renderers can
                        // resolve the failing job + log via `gh api`.
                        // See issue #303: previous behavior dropped this so
                        // `ship.rs:dispatched_run` had to overwrite with the
                        // internal Shipyard job id, which was useless for
                        // failure diagnostics.
                        if !passed {
                            result.cloud_run_id = Some(run.database_id);
                        }
                        return result;
                    }
                }
                Err(error) => {
                    return terminal_result(
                        &request.target,
                        started_at,
                        started,
                        &request.log_path,
                        TargetStatus::Error,
                        Some(format!("Failed to poll workflow run: {error}")),
                        Some(FailureClass::Infra),
                    );
                }
            }
            if Instant::now() >= deadline {
                return terminal_result(
                    &request.target,
                    started_at,
                    started,
                    &request.log_path,
                    TargetStatus::Error,
                    Some(format!(
                        "Cloud workflow run {run_id} did not complete within {}s",
                        request.target.max_poll_secs
                    )),
                    Some(FailureClass::Timeout),
                );
            }
            sleep_poll(
                request.target.poll_interval_secs,
                request.target.poll_interval_secs,
            );
        }
    }
}

fn dispatch_fields(target: &CloudTargetConfig) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    if let Some(provider) = target
        .runner_provider
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        fields.insert("runner_provider".to_owned(), provider.to_owned());
    }
    if let Some(selector) = target
        .runner_selector
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        fields.insert("runner_selector".to_owned(), selector.to_owned());
    }
    if !target.runner_overrides.is_empty()
        && let Ok(encoded) = serde_json::to_string(&target.runner_overrides)
    {
        fields.insert("runner_overrides".to_owned(), encoded);
    }
    fields
}

fn terminal_result(
    target: &CloudTargetConfig,
    started_at: DateTime<Utc>,
    started: Instant,
    log_path: &Path,
    status: TargetStatus,
    error_message: Option<String>,
    failure_class: Option<FailureClass>,
) -> TargetResult {
    let completed_at = Utc::now();
    let mut result = TargetResult::new(&target.name, &target.platform, status, "cloud");
    result.started_at = Some(started_at);
    result.completed_at = Some(completed_at);
    result.duration_secs = Some(started.elapsed().as_secs_f64());
    result.log_path = Some(log_path.to_string_lossy().into_owned());
    result.provider.clone_from(&target.runner_provider);
    result.runner_profile.clone_from(&target.runner_selector);
    result.last_heartbeat_at = Some(completed_at);
    result.error_message = error_message;
    result.failure_class = failure_class.map(|class| class.as_str().to_owned());
    // Issue #303: thread the per-target failure parser selection through to
    // the target result so the renderer in `ship_cmd.rs` knows which parser
    // to pick when diagnosing the failure.
    result.failure_parser.clone_from(&target.failure_parser);
    result
}

fn emit_progress(
    progress_callback: &mut Option<
        &mut dyn FnMut(ProgressEvent) -> crate::executor::streaming::ProgressAction,
    >,
    phase: impl Into<String>,
) {
    if let Some(callback) = progress_callback.as_deref_mut() {
        callback(ProgressEvent::phase(phase));
    }
}

fn sleep_poll(poll_interval_secs: u64, cap_secs: u64) {
    let secs = poll_interval_secs.min(cap_secs);
    if secs > 0 {
        std::thread::sleep(Duration::from_secs(secs));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};
    use std::rc::Rc;

    use super::{
        CloudActionsClient, CloudExecutor, CloudTargetConfig, CloudValidationRequest, WorkflowRun,
    };
    use crate::cloud::GitHubError;
    use crate::job::TargetStatus;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct DispatchRecord {
        repository: Option<String>,
        workflow_file: String,
        ref_name: String,
        fields: BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    struct FakeClient {
        auth: bool,
        dispatch_error: Option<GitHubError>,
        dispatches: Rc<RefCell<Vec<DispatchRecord>>>,
        latest: Rc<RefCell<VecDeque<Option<WorkflowRun>>>>,
        statuses: Rc<RefCell<VecDeque<WorkflowRun>>>,
        polled_run_ids: Rc<RefCell<Vec<u64>>>,
    }

    impl FakeClient {
        fn authenticated() -> Self {
            Self {
                auth: true,
                ..Self::default()
            }
        }

        fn push_latest(&self, run: Option<WorkflowRun>) {
            self.latest.borrow_mut().push_back(run);
        }

        fn push_status(&self, run: WorkflowRun) {
            self.statuses.borrow_mut().push_back(run);
        }
    }

    impl CloudActionsClient for FakeClient {
        fn auth_status(&self) -> bool {
            self.auth
        }

        fn workflow_dispatch(
            &self,
            repository: Option<&str>,
            workflow_file: &str,
            ref_name: &str,
            fields: &BTreeMap<String, String>,
        ) -> Result<(), GitHubError> {
            if let Some(error) = &self.dispatch_error {
                return Err(error.clone());
            }
            self.dispatches.borrow_mut().push(DispatchRecord {
                repository: repository.map(ToOwned::to_owned),
                workflow_file: workflow_file.to_owned(),
                ref_name: ref_name.to_owned(),
                fields: fields.clone(),
            });
            Ok(())
        }

        fn latest_workflow_run_for_branch(
            &self,
            _repository: Option<&str>,
            _workflow_file: &str,
            _branch: &str,
        ) -> Result<Option<WorkflowRun>, GitHubError> {
            Ok(self.latest.borrow_mut().pop_front().flatten())
        }

        fn workflow_run_status(
            &self,
            _repository: Option<&str>,
            run_id: u64,
        ) -> Result<WorkflowRun, GitHubError> {
            self.polled_run_ids.borrow_mut().push(run_id);
            self.statuses
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| GitHubError::new("no fake status queued"))
        }
    }

    #[test]
    fn cloud_executor_dispatches_namespace_and_returns_pass() {
        let client = FakeClient::authenticated();
        client.push_latest(None);
        client.push_latest(Some(run(42, "queued", None)));
        client.push_status(run(42, "completed", Some("success")));
        let executor = CloudExecutor::with_client(client.clone());
        let mut target = CloudTargetConfig::new("linux", "linux-x64");
        target.workflow = "ci.yml".to_owned();
        target.repository = Some("owner/repo".to_owned());
        target.runner_provider = Some("namespace".to_owned());
        target.runner_selector = Some("namespace-profile-generouscorp".to_owned());
        target.runner_overrides.insert(
            "linux-x64".to_owned(),
            "namespace-profile-generouscorp".to_owned(),
        );
        target.poll_interval_secs = 0;
        target.dispatch_settle_secs = 0;
        target.max_poll_secs = 0;
        let mut request = CloudValidationRequest::new("cloud.log".into(), target);
        request.branch = "feature/x".to_owned();
        request.sha = "abc123".to_owned();
        let mut phases = Vec::new();
        let mut progress = |event: crate::executor::streaming::ProgressEvent| {
            phases.push(event.phase.expect("phase"));
            crate::executor::streaming::ProgressAction::Continue
        };
        request.progress_callback = Some(&mut progress);

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Pass);
        assert_eq!(result.backend, "cloud");
        assert_eq!(result.provider.as_deref(), Some("namespace"));
        assert_eq!(
            result.runner_profile.as_deref(),
            Some("namespace-profile-generouscorp")
        );
        assert!(result.failure_class.is_none());
        assert_eq!(
            phases,
            vec!["dispatch", "queued", "completed"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );
        let dispatches = client.dispatches.borrow();
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].repository.as_deref(), Some("owner/repo"));
        assert_eq!(dispatches[0].workflow_file, "ci.yml");
        assert_eq!(dispatches[0].ref_name, "feature/x");
        assert_eq!(dispatches[0].fields["runner_provider"], "namespace");
        assert_eq!(
            dispatches[0].fields["runner_selector"],
            "namespace-profile-generouscorp"
        );
        assert!(
            dispatches[0].fields["runner_overrides"].contains("namespace-profile-generouscorp")
        );
    }

    #[test]
    fn cloud_executor_maps_failed_conclusion_to_test_failure() {
        let client = FakeClient::authenticated();
        client.push_latest(None);
        client.push_latest(Some(run(7, "queued", None)));
        client.push_status(run(7, "completed", Some("failure")));
        let executor = CloudExecutor::with_client(client);
        let mut target = CloudTargetConfig::new("linux", "linux-x64");
        target.poll_interval_secs = 0;
        target.dispatch_settle_secs = 0;
        target.max_poll_secs = 0;
        let mut request = CloudValidationRequest::new("cloud.log".into(), target);
        request.branch = "feature/x".to_owned();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Fail);
        assert_eq!(result.failure_class.as_deref(), Some("TEST"));
    }

    #[test]
    fn cloud_executor_times_out_when_run_never_appears() {
        let client = FakeClient::authenticated();
        client.push_latest(None);
        client.push_latest(None);
        let executor = CloudExecutor::with_client(client);
        let mut target = CloudTargetConfig::new("linux", "linux-x64");
        target.poll_interval_secs = 0;
        target.dispatch_settle_secs = 0;
        let mut request = CloudValidationRequest::new("cloud.log".into(), target);
        request.branch = "feature/x".to_owned();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Error);
        assert_eq!(result.failure_class.as_deref(), Some("TIMEOUT"));
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("did not appear within 0s"))
        );
    }

    #[test]
    fn cloud_executor_ignores_stale_pre_dispatch_run() {
        let client = FakeClient::authenticated();
        client.push_latest(Some(run(10, "completed", Some("success"))));
        client.push_latest(Some(run(10, "completed", Some("success"))));
        client.push_latest(Some(run(11, "queued", None)));
        client.push_status(run(11, "completed", Some("success")));
        let executor = CloudExecutor::with_client(client.clone());
        let mut target = CloudTargetConfig::new("linux", "linux-x64");
        target.poll_interval_secs = 0;
        target.dispatch_settle_secs = 1;
        target.max_poll_secs = 0;
        let mut request = CloudValidationRequest::new("cloud.log".into(), target);
        request.branch = "feature/x".to_owned();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Pass);
        assert_eq!(*client.polled_run_ids.borrow(), vec![11]);
    }

    #[test]
    fn cloud_probe_uses_gh_auth_status() {
        assert!(CloudExecutor::with_client(FakeClient::authenticated()).probe());
        assert!(!CloudExecutor::with_client(FakeClient::default()).probe());
    }

    fn run(database_id: u64, status: &str, conclusion: Option<&str>) -> WorkflowRun {
        WorkflowRun {
            database_id,
            status: status.to_owned(),
            conclusion: conclusion.map(ToOwned::to_owned),
            url: None,
        }
    }
}
