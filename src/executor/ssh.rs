use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use wait_timeout::ChildExt;

use crate::bundle::git_bundle::{
    BundleResult, apply_bundle_posix, create_bundle, upload_bundle_posix,
};
use crate::classify::{FailureClass, classify_failure};
use crate::executor::contract::{ContractConfig, evaluate_contract, required_markers};
use crate::executor::streaming::{
    ProgressEvent, StreamingCommand, StreamingCommandResult, StreamingCommandSpec, StreamingError,
    run_streaming_command,
};
use crate::job::{TargetResult, TargetStatus};

const STAGE_ORDER: [&str; 4] = ["setup", "configure", "build", "test"];
const PROBE_CONNECT_TIMEOUT_SECS: u64 = 5;
const PROBE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_RETRY_BACKOFFS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(6)];
const DEFAULT_TIMEOUT_SECS: u64 = 1_800;
const DEFAULT_BUNDLE_TIMEOUT_SECS: u64 = 1_800;
const SSH_RETRY_BACKOFFS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
const TRANSIENT_SSH_PATTERNS: [&str; 10] = [
    "connection reset by peer",
    "kex_exchange_identification",
    "connection closed by remote host",
    "connection timed out",
    "ssh_exchange_identification",
    "connection refused",
    "network is unreachable",
    "no route to host",
    "broken pipe",
    "operation timed out",
];

/// Validation script shape for a POSIX SSH target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SshValidation {
    /// A single validation command string.
    Command(String),
    /// Ordered validation stages keyed by Shipyard's stage names.
    Stages(BTreeMap<String, String>),
}

/// Target settings for a POSIX SSH validation run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshTargetConfig {
    /// Target name.
    pub name: String,
    /// Platform label.
    pub platform: String,
    /// SSH host, including optional user.
    pub host: Option<String>,
    /// Remote git checkout path.
    pub repo_path: String,
    /// Configured SSH options.
    pub ssh_options: Vec<String>,
    /// Optional identity file appended to SSH options.
    pub identity_file: Option<String>,
    /// Remote path used for bundle upload.
    pub remote_bundle_path: String,
    /// Optional local repository directory used for bundle creation.
    pub local_repo_dir: Option<PathBuf>,
    /// Validation timeout in seconds.
    pub timeout_secs: u64,
    /// Bundle upload timeout in seconds.
    pub bundle_upload_timeout_secs: u64,
    /// Bundle apply timeout in seconds.
    pub bundle_apply_timeout_secs: u64,
}

impl Default for SshTargetConfig {
    fn default() -> Self {
        Self {
            name: "ssh".to_owned(),
            platform: "unknown".to_owned(),
            host: None,
            repo_path: "~/repo".to_owned(),
            ssh_options: Vec::new(),
            identity_file: None,
            remote_bundle_path: "/tmp/shipyard.bundle".to_owned(),
            local_repo_dir: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            bundle_upload_timeout_secs: DEFAULT_BUNDLE_TIMEOUT_SECS,
            bundle_apply_timeout_secs: DEFAULT_BUNDLE_TIMEOUT_SECS,
        }
    }
}

impl SshTargetConfig {
    /// Return the validation timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    /// Return the bundle upload timeout.
    #[must_use]
    pub fn bundle_upload_timeout(&self) -> Duration {
        Duration::from_secs(self.bundle_upload_timeout_secs)
    }

    /// Return the bundle apply timeout.
    #[must_use]
    pub fn bundle_apply_timeout(&self) -> Duration {
        Duration::from_secs(self.bundle_apply_timeout_secs)
    }
}

/// Request for one POSIX SSH validation run.
pub struct SshValidationRequest<'a> {
    /// Commit SHA under validation.
    pub sha: String,
    /// Branch under validation.
    pub branch: String,
    /// Target execution settings.
    pub target: SshTargetConfig,
    /// Validation command/stage settings.
    pub validation: SshValidation,
    /// Optional validation contract.
    pub contract: Option<ContractConfig>,
    /// Local log file path.
    pub log_path: PathBuf,
    /// Number of prior target-log segments preserved on reopen.
    pub rotated_segments: usize,
    /// Optional stage to resume from.
    pub resume_from: Option<String>,
    /// Validation mode label.
    pub mode: String,
    /// Optional progress callback.
    pub progress_callback:
        Option<&'a mut dyn FnMut(ProgressEvent) -> crate::executor::streaming::ProgressAction>,
}

impl SshValidationRequest<'_> {
    /// Create a request with Python-compatible defaults.
    #[must_use]
    pub fn new(log_path: PathBuf, validation: SshValidation) -> Self {
        Self {
            sha: String::new(),
            branch: String::new(),
            target: SshTargetConfig::default(),
            validation,
            contract: None,
            log_path,
            rotated_segments: crate::log_retention::LogRetentionPolicy::default().rotated_segments,
            resume_from: None,
            mode: "default".to_owned(),
            progress_callback: None,
        }
    }
}

/// Side-effect boundary used by the POSIX SSH executor.
///
/// Unit tests use this trait to prove orchestration without requiring a
/// reachable remote host; production uses [`SystemSshOperations`].
pub trait SshOperations {
    /// Return true when the remote repository already has `sha`.
    fn remote_has_sha(
        &self,
        host: &str,
        repo_path: &str,
        sha: &str,
        ssh_options: &[String],
    ) -> bool;

    /// Return the remote repository HEAD SHA when it can be queried.
    fn remote_head_sha(
        &self,
        host: &str,
        repo_path: &str,
        ssh_options: &[String],
    ) -> Option<String>;

    /// Return true when the local object store has a candidate basis SHA.
    fn local_has_commit(&self, sha: &str, repo_dir: Option<&Path>) -> bool;

    /// Create a bundle for delivery to the remote.
    fn create_bundle(
        &self,
        sha: &str,
        output_path: &Path,
        repo_dir: Option<&Path>,
        basis_shas: &[String],
    ) -> BundleResult;

    /// Upload a bundle to a POSIX remote host.
    fn upload_bundle(
        &self,
        bundle_path: &Path,
        host: &str,
        remote_path: &str,
        ssh_options: &[String],
        timeout: Duration,
    ) -> BundleResult;

    /// Apply a bundle on a POSIX remote host.
    fn apply_bundle(
        &self,
        host: &str,
        bundle_path: &str,
        repo_path: &str,
        ssh_options: &[String],
        timeout: Duration,
    ) -> BundleResult;

    /// Return true when a remote stage marker exists.
    fn remote_marker_exists(
        &self,
        host: &str,
        repo_path: &str,
        marker: &str,
        ssh_options: &[String],
    ) -> bool;

    /// Run the final validation command through the streaming layer.
    fn run_streaming_command(
        &self,
        request: StreamingCommand<'_>,
    ) -> Result<StreamingCommandResult, StreamingError>;

    /// Sleep between transient SSH retries.
    fn sleep(&self, duration: Duration);
}

/// Production SSH operations backed by `ssh`, `git`, and git bundles.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSshOperations;

impl SshOperations for SystemSshOperations {
    fn remote_has_sha(
        &self,
        host: &str,
        repo_path: &str,
        sha: &str,
        ssh_options: &[String],
    ) -> bool {
        let command = format!(
            "cd {} && git cat-file -e {}",
            shlex_quote(repo_path),
            shlex_quote(sha)
        );
        run_capture(
            ssh_capture_command(host, ssh_options, &command),
            Duration::from_secs(15),
        )
        .is_ok_and(|output| output.success())
    }

    fn remote_head_sha(
        &self,
        host: &str,
        repo_path: &str,
        ssh_options: &[String],
    ) -> Option<String> {
        let command = format!("cd {} && git rev-parse HEAD", shlex_quote(repo_path));
        let output = run_capture(
            ssh_capture_command(host, ssh_options, &command),
            Duration::from_secs(15),
        )
        .ok()?;
        if !output.success() {
            return None;
        }
        let sha = output.stdout.trim();
        (looks_like_sha(sha)).then(|| sha.to_owned())
    }

    fn local_has_commit(&self, sha: &str, repo_dir: Option<&Path>) -> bool {
        let mut command = Command::new("git");
        command.args(["cat-file", "-e", &format!("{sha}^{{commit}}")]);
        if let Some(repo_dir) = repo_dir {
            command.current_dir(repo_dir);
        }
        run_capture(command, Duration::from_secs(10)).is_ok_and(|output| output.success())
    }

    fn create_bundle(
        &self,
        sha: &str,
        output_path: &Path,
        repo_dir: Option<&Path>,
        basis_shas: &[String],
    ) -> BundleResult {
        create_bundle(sha, output_path, repo_dir, basis_shas)
    }

    fn upload_bundle(
        &self,
        bundle_path: &Path,
        host: &str,
        remote_path: &str,
        ssh_options: &[String],
        timeout: Duration,
    ) -> BundleResult {
        upload_bundle_posix(bundle_path, host, remote_path, ssh_options, timeout)
    }

    fn apply_bundle(
        &self,
        host: &str,
        bundle_path: &str,
        repo_path: &str,
        ssh_options: &[String],
        timeout: Duration,
    ) -> BundleResult {
        apply_bundle_posix(host, bundle_path, repo_path, ssh_options, timeout)
    }

    fn remote_marker_exists(
        &self,
        host: &str,
        repo_path: &str,
        marker: &str,
        ssh_options: &[String],
    ) -> bool {
        let command = format!("test -f {}/{}", shlex_quote(repo_path), shlex_quote(marker));
        run_capture(
            ssh_capture_command(host, ssh_options, &command),
            Duration::from_secs(15),
        )
        .is_ok_and(|output| output.success())
    }

    fn run_streaming_command(
        &self,
        request: StreamingCommand<'_>,
    ) -> Result<StreamingCommandResult, StreamingError> {
        run_streaming_command(request)
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Execute validation commands on POSIX SSH targets.
#[derive(Clone, Debug)]
pub struct SshExecutor<O = SystemSshOperations> {
    operations: O,
    max_retries: usize,
}

impl Default for SshExecutor<SystemSshOperations> {
    fn default() -> Self {
        Self::new()
    }
}

impl SshExecutor<SystemSshOperations> {
    /// Construct a production SSH executor.
    #[must_use]
    pub fn new() -> Self {
        Self::with_operations(SystemSshOperations)
    }
}

impl<O: SshOperations> SshExecutor<O> {
    /// Construct an executor using injected operations.
    #[must_use]
    pub fn with_operations(operations: O) -> Self {
        Self {
            operations,
            max_retries: SSH_RETRY_BACKOFFS.len(),
        }
    }

    /// Override the retry count for tests or controlled callers.
    #[must_use]
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Run a POSIX SSH validation request and return a target result.
    #[must_use]
    pub fn validate(&self, mut request: SshValidationRequest<'_>) -> TargetResult {
        let mut progress_callback = request.progress_callback.take();
        for attempt in 0..=self.max_retries {
            let callback = progress_callback.as_mut().map(|callback| {
                &mut **callback
                    as &mut dyn FnMut(ProgressEvent) -> crate::executor::streaming::ProgressAction
            });
            let result = self.validate_once(&request, callback);
            let retryable = result
                .error_message
                .as_deref()
                .is_some_and(is_transient_ssh_error);
            if result.status == TargetStatus::Error && retryable && attempt < self.max_retries {
                let backoff = SSH_RETRY_BACKOFFS
                    .get(attempt)
                    .copied()
                    .unwrap_or_else(|| *SSH_RETRY_BACKOFFS.last().expect("backoff"));
                self.operations.sleep(backoff);
                continue;
            }
            return result;
        }
        unreachable!("retry loop always returns a result")
    }

    #[allow(clippy::too_many_lines)]
    fn validate_once(
        &self,
        request: &SshValidationRequest<'_>,
        progress_callback: Option<
            &mut dyn FnMut(ProgressEvent) -> crate::executor::streaming::ProgressAction,
        >,
    ) -> TargetResult {
        let started_at = Utc::now();
        let start_time = Instant::now();
        let context = SshRunContext {
            target: &request.target,
            log_path: &request.log_path,
            started_at,
            start_time,
        };
        if let Some(parent) = request.log_path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            return ssh_error_result(&context, &error.to_string());
        }

        let Some(host) = request
            .target
            .host
            .as_deref()
            .filter(|host| !host.trim().is_empty())
        else {
            return ssh_error_result(
                &context,
                &format!(
                    "Target '{}' is misconfigured: no `host` field in .shipyard/config.toml or .shipyard.local/config.toml.",
                    request.target.name
                ),
            );
        };

        let ssh_options = ssh_options(
            &request.target.ssh_options,
            request.target.identity_file.as_deref(),
        );

        if !self.operations.remote_has_sha(
            host,
            &request.target.repo_path,
            &request.sha,
            &ssh_options,
        ) {
            let bundle_result = self.deliver_bundle(&context, request, host, &ssh_options);
            if let Err(message) = bundle_result {
                return ssh_error_result(&context, &message);
            }
        }

        let effective_resume = resolve_resume_from(
            &ResumeProbe {
                operations: &self.operations,
                host,
                remote_repo: &request.target.repo_path,
                sha: &request.sha,
                ssh_options: &ssh_options,
                validation: &request.validation,
                log_file: &request.log_path,
            },
            request.resume_from.as_deref(),
        );

        let Some(remote_command) = build_remote_command(
            &request.sha,
            &request.target.repo_path,
            &request.validation,
            effective_resume.as_deref(),
        ) else {
            return ssh_error_result(&context, "No validation command configured");
        };

        let mut argv = Vec::with_capacity(3 + ssh_options.len());
        argv.push("ssh".to_owned());
        argv.extend(ssh_options);
        argv.push(host.to_owned());
        argv.push(remote_command);

        let mut stream_request = StreamingCommand::shell(String::new());
        stream_request.command = StreamingCommandSpec::Args(argv);
        stream_request.log_path = Some(request.log_path.clone());
        stream_request.rotated_segments = request.rotated_segments;
        stream_request.timeout = Some(request.target.timeout());
        stream_request.required_contract_markers = required_markers(request.contract.as_ref());
        stream_request.progress_callback = progress_callback;

        match self.operations.run_streaming_command(stream_request) {
            Ok(result) => ssh_command_result(&context, request.contract.as_ref(), result),
            Err(error) => streaming_error_result(&context, error),
        }
    }

    fn deliver_bundle(
        &self,
        context: &SshRunContext<'_>,
        request: &SshValidationRequest<'_>,
        host: &str,
        ssh_options: &[String],
    ) -> Result<(), String> {
        let temp = tempfile::tempdir().map_err(|error| format!("OS error: {error}"))?;
        let bundle_path = temp.path().join("shipyard.bundle");
        let remote_head =
            self.operations
                .remote_head_sha(host, &request.target.repo_path, ssh_options);
        let mut basis_shas = Vec::new();
        if let Some(remote_head) = &remote_head
            && self
                .operations
                .local_has_commit(remote_head, request.target.local_repo_dir.as_deref())
        {
            basis_shas.push(remote_head.clone());
        }

        let mut bundle_mode = "full";
        let mut bundle_result = self.operations.create_bundle(
            &request.sha,
            &bundle_path,
            request.target.local_repo_dir.as_deref(),
            &basis_shas,
        );
        if bundle_result.success && !basis_shas.is_empty() {
            bundle_mode = "delta";
        }
        if !bundle_result.success && !basis_shas.is_empty() {
            bundle_result = self.operations.create_bundle(
                &request.sha,
                &bundle_path,
                request.target.local_repo_dir.as_deref(),
                &[],
            );
            bundle_mode = "full";
        }
        if !bundle_result.success {
            return Err(format!("Bundle creation failed: {}", bundle_result.message));
        }

        let bundle_bytes = safe_filesize(&bundle_path);
        let _ = append_log(
            context.log_path,
            &format!(
                "=== bundle_mode={bundle_mode} bundle_bytes={bundle_bytes} sha={} remote_head={} ===\n",
                request.sha,
                remote_head.as_deref().unwrap_or("unknown")
            ),
        );

        let upload_result = self.operations.upload_bundle(
            &bundle_path,
            host,
            &request.target.remote_bundle_path,
            ssh_options,
            request.target.bundle_upload_timeout(),
        );
        if !upload_result.success {
            return Err(format!("Bundle upload failed: {}", upload_result.message));
        }

        let apply_result = self.operations.apply_bundle(
            host,
            &request.target.remote_bundle_path,
            &request.target.repo_path,
            ssh_options,
            request.target.bundle_apply_timeout(),
        );
        if !apply_result.success {
            return Err(format!("Bundle apply failed: {}", apply_result.message));
        }
        Ok(())
    }
}

/// Stable probe failure buckets surfaced by preflight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeCategory {
    /// Authentication or key rejection.
    Auth,
    /// Host key mismatch or verification failure.
    HostKey,
    /// DNS or host resolution failure.
    Resolution,
    /// Network path or connection failure.
    Network,
    /// Probe timeout.
    Timeout,
    /// Missing target host or similar configuration problem.
    Configuration,
    /// Unknown SSH failure.
    Unknown,
}

impl ProbeCategory {
    /// Return the Python-compatible category string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::HostKey => "host_key",
            Self::Resolution => "resolution",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Configuration => "configuration",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ProbeCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Probe result fields used to format a preflight diagnosis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeDiagnostic {
    /// Host from target config, if present.
    pub host: Option<String>,
    /// Optional configured SSH port.
    pub port: Option<u16>,
    /// Stable failure category.
    pub category: Option<ProbeCategory>,
    /// Number of probe attempts performed.
    pub attempts: usize,
    /// Last probe error line.
    pub last_error: Option<String>,
}

/// Reachability outcome from an SSH preflight probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeOutcome {
    /// Whether the target accepted a simple SSH command.
    pub reachable: bool,
    /// Diagnostic fields used for failure rendering.
    pub diagnostic: ProbeDiagnostic,
}

/// Quote a string for POSIX shell usage, matching Python's
/// `shlex.quote` behavior for the path shapes Shipyard emits.
#[must_use]
pub fn shlex_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'))
    {
        return value.to_owned();
    }

    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

/// Build the remote POSIX shell validation command.
///
/// Returns `None` when a staged validation config has no active stages.
#[must_use]
pub fn build_remote_command(
    sha: &str,
    remote_repo: &str,
    validation: &SshValidation,
    resume_from: Option<&str>,
) -> Option<String> {
    let validate_cmd = match validation {
        SshValidation::Command(command) => command.clone(),
        SshValidation::Stages(stages) => build_staged_validation(sha, stages, resume_from)?,
    };

    Some(format!(
        "cd {} && git checkout --force {} && {validate_cmd}",
        shlex_quote(remote_repo),
        shlex_quote(sha)
    ))
}

/// Build the `ssh` argv for a reachability probe.
#[must_use]
pub fn build_probe_command(
    host: &str,
    ssh_options: &[String],
    remote_cmd: &[String],
) -> Vec<String> {
    let mut command = Vec::with_capacity(7 + ssh_options.len() + remote_cmd.len());
    command.push("ssh".to_owned());
    command.extend_from_slice(ssh_options);
    command.push("-o".to_owned());
    command.push(format!("ConnectTimeout={PROBE_CONNECT_TIMEOUT_SECS}"));
    command.push("-o".to_owned());
    command.push("BatchMode=yes".to_owned());
    command.push(host.to_owned());
    command.extend_from_slice(remote_cmd);
    command
}

/// Probe one POSIX SSH target for preflight reachability.
#[must_use]
pub fn probe_target(target: &SshTargetConfig) -> bool {
    diagnose_target(target).reachable
}

/// Diagnose one POSIX SSH target for preflight reachability.
#[must_use]
pub fn diagnose_target(target: &SshTargetConfig) -> ProbeOutcome {
    diagnose_target_with(target, run_probe_argv, std::thread::sleep)
}

/// Extract SSH options from config-shaped values.
#[must_use]
pub fn ssh_options(configured_options: &[String], identity_file: Option<&str>) -> Vec<String> {
    let mut options = configured_options.to_vec();
    if let Some(identity_file) = identity_file {
        options.extend(["-i".to_owned(), identity_file.to_owned()]);
    }
    options
}

/// Classify one failed probe attempt.
#[must_use]
pub fn classify_probe_error(stderr: &str, returncode: i32) -> ProbeCategory {
    let lowered = stderr.to_lowercase();
    if lowered.contains("permission denied")
        || lowered.contains("publickey")
        || lowered.contains("too many authentication failures")
    {
        return ProbeCategory::Auth;
    }
    if lowered.contains("host key verification failed")
        || lowered.contains("remote host identification has changed")
        || lowered.contains("offending")
    {
        return ProbeCategory::HostKey;
    }
    if lowered.contains("could not resolve hostname")
        || lowered.contains("name or service not known")
        || lowered.contains("nodename nor servname")
    {
        return ProbeCategory::Resolution;
    }
    if lowered.contains("no route to host")
        || lowered.contains("network is unreachable")
        || lowered.contains("connection refused")
    {
        return ProbeCategory::Network;
    }
    if lowered.contains("connection timed out") {
        return ProbeCategory::Timeout;
    }
    if returncode == 255 {
        return ProbeCategory::Network;
    }
    ProbeCategory::Unknown
}

/// Format the preflight diagnosis shown for an unreachable SSH target.
#[must_use]
pub fn format_ssh_diagnosis(diagnostic: &ProbeDiagnostic) -> String {
    let host_raw = diagnostic.host.as_deref();
    let mut user_at_host = host_raw.unwrap_or("<no host>").to_owned();
    if let Some(port) = diagnostic.port {
        user_at_host = format!("{user_at_host}:{port}");
    }

    let mut lines = vec![
        format!("SSH backend unreachable at {user_at_host}."),
        "  Attempted: 10s probe with ConnectTimeout=5, BatchMode=yes.".to_owned(),
    ];

    if let Some(category) = diagnostic.category {
        lines.push(format!("  Failure category: {category}"));
    }
    if diagnostic.attempts > 1 {
        lines.push(format!("  Attempts: {}", diagnostic.attempts));
    }
    if let Some(last_error) = &diagnostic.last_error
        && !last_error.is_empty()
    {
        lines.push(format!("  Last error: {last_error}"));
    }

    if host_raw.is_none() || diagnostic.category == Some(ProbeCategory::Configuration) {
        lines.extend([
            String::new(),
            "  Hint: the target has no host configured. If you're running from a git worktree, the gitignored .shipyard.local/config.toml from the main checkout wasn't copied over. Copy it in:".to_owned(),
            "    cp -r <main-checkout>/.shipyard.local ./".to_owned(),
            "  shipyard now auto-discovers it too when one exists - re-run from a terminal in this worktree to trigger the fallback lookup.".to_owned(),
        ]);
    }

    lines.join("\n")
}

/// Return whether an SSH error is worth retrying with backoff.
#[must_use]
pub fn is_transient_ssh_error(message: &str) -> bool {
    let lowered = message.to_lowercase();
    TRANSIENT_SSH_PATTERNS
        .iter()
        .any(|pattern| lowered.contains(pattern))
}

fn diagnose_target_with<R, S>(
    target: &SshTargetConfig,
    mut runner: R,
    mut sleeper: S,
) -> ProbeOutcome
where
    R: FnMut(&[String], Duration) -> io::Result<CommandCapture>,
    S: FnMut(Duration),
{
    let Some(host) = target.host.as_deref() else {
        return ProbeOutcome {
            reachable: false,
            diagnostic: ProbeDiagnostic {
                host: None,
                port: configured_port(&target.ssh_options),
                category: Some(ProbeCategory::Configuration),
                attempts: 0,
                last_error: Some("target has no host configured".to_owned()),
            },
        };
    };

    let ssh_options = ssh_options(&target.ssh_options, target.identity_file.as_deref());
    let argv = build_probe_command(host, &ssh_options, &["echo".to_owned(), "ok".to_owned()]);
    let mut attempts = 0;

    loop {
        attempts += 1;
        let (last_error, last_category) = match runner(&argv, PROBE_ATTEMPT_TIMEOUT) {
            Ok(output) if output.success() => {
                return ProbeOutcome {
                    reachable: true,
                    diagnostic: ProbeDiagnostic {
                        host: Some(host.to_owned()),
                        port: configured_port(&target.ssh_options),
                        category: None,
                        attempts,
                        last_error: None,
                    },
                };
            }
            Ok(output) => {
                let error = probe_error_message(&output);
                let category = if output.timed_out {
                    ProbeCategory::Timeout
                } else {
                    classify_probe_error(&error, output.returncode.unwrap_or(-1))
                };
                if should_retry_probe(category)
                    && let Some(backoff) = PROBE_RETRY_BACKOFFS.get(attempts - 1)
                {
                    sleeper(*backoff);
                    continue;
                }
                (Some(error), Some(category))
            }
            Err(error) => (Some(error.to_string()), Some(ProbeCategory::Unknown)),
        };

        return ProbeOutcome {
            reachable: false,
            diagnostic: ProbeDiagnostic {
                host: Some(host.to_owned()),
                port: configured_port(&target.ssh_options),
                category: last_category,
                attempts,
                last_error,
            },
        };
    }
}

fn run_probe_argv(argv: &[String], timeout: Duration) -> io::Result<CommandCapture> {
    let mut command = Command::new(
        argv.first()
            .expect("probe argv always includes executable name"),
    );
    command.args(&argv[1..]);
    run_capture(command, timeout)
}

fn probe_error_message(output: &CommandCapture) -> String {
    if output.timed_out {
        return "timed out after 10s".to_owned();
    }
    let stderr = output.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_owned();
    }
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_owned();
    }
    format!("ssh exited with code {}", output.returncode.unwrap_or(-1))
}

fn should_retry_probe(category: ProbeCategory) -> bool {
    matches!(category, ProbeCategory::Network | ProbeCategory::Timeout)
}

fn configured_port(options: &[String]) -> Option<u16> {
    options.windows(2).find_map(|window| {
        (window[0] == "-p" || window[0] == "Port").then(|| window[1].parse().ok())?
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandCapture {
    returncode: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

impl CommandCapture {
    fn success(&self) -> bool {
        self.returncode == Some(0) && !self.timed_out
    }
}

fn run_capture(mut command: Command, timeout: Duration) -> io::Result<CommandCapture> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let timed_out = child.wait_timeout(timeout)?.is_none();
    if timed_out {
        let _ = child.kill();
    }
    let output = child.wait_with_output()?;
    Ok(CommandCapture {
        returncode: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        timed_out,
    })
}

fn ssh_capture_command(host: &str, ssh_options: &[String], remote_command: &str) -> Command {
    let mut command = Command::new("ssh");
    command.args(ssh_options);
    command.args([
        "-o",
        &format!("ConnectTimeout={PROBE_CONNECT_TIMEOUT_SECS}"),
    ]);
    command.arg(host);
    command.arg(remote_command);
    command
}

fn looks_like_sha(value: &str) -> bool {
    value.len() >= 7 && value.chars().all(|character| character.is_ascii_hexdigit())
}

struct ResumeProbe<'a, O> {
    operations: &'a O,
    host: &'a str,
    remote_repo: &'a str,
    sha: &'a str,
    ssh_options: &'a [String],
    validation: &'a SshValidation,
    log_file: &'a Path,
}

fn resolve_resume_from<O: SshOperations>(
    probe: &ResumeProbe<'_, O>,
    requested: Option<&str>,
) -> Option<String> {
    let requested = requested?;
    let SshValidation::Stages(stages) = probe.validation else {
        let _ = append_log(
            probe.log_file,
            &format!(
                "=== resume-from: ignored ({requested:?}) - validation_config uses single command, no stages to skip ===\n"
            ),
        );
        return None;
    };
    let Some(index) = STAGE_ORDER.iter().position(|stage| *stage == requested) else {
        let _ = append_log(
            probe.log_file,
            &format!("=== resume-from: ignored - unknown stage {requested:?} ===\n"),
        );
        return None;
    };
    if index == 0 {
        return Some(requested.to_owned());
    }

    let previous_stage = STAGE_ORDER[..index]
        .iter()
        .rev()
        .find(|stage| stages.contains_key(**stage));
    let Some(previous_stage) = previous_stage else {
        return Some(requested.to_owned());
    };

    let marker = stage_marker_name(previous_stage, probe.sha);
    if probe.operations.remote_marker_exists(
        probe.host,
        probe.remote_repo,
        &marker,
        probe.ssh_options,
    ) {
        let _ = append_log(
            probe.log_file,
            &format!(
                "=== resume-from: honoring {requested:?} - found marker for previous stage {previous_stage:?} on remote ===\n"
            ),
        );
        return Some(requested.to_owned());
    }

    let _ = append_log(
        probe.log_file,
        &format!(
            "=== resume-from: requested {requested:?} but marker for previous stage {previous_stage:?} not found on remote - running all stages from the beginning ===\n"
        ),
    );
    None
}

struct SshRunContext<'a> {
    target: &'a SshTargetConfig,
    log_path: &'a Path,
    started_at: DateTime<Utc>,
    start_time: Instant,
}

fn ssh_command_result(
    context: &SshRunContext<'_>,
    contract: Option<&ContractConfig>,
    result: StreamingCommandResult,
) -> TargetResult {
    let mut status = if result.returncode == 0 {
        TargetStatus::Pass
    } else {
        TargetStatus::Fail
    };
    let mut error_message = None;
    if result.returncode == 255 {
        status = TargetStatus::Error;
        error_message = Some(
            extract_ssh_error(&result.output).unwrap_or_else(|| "SSH transport failed".to_owned()),
        );
    }

    let evaluation = evaluate_contract(contract, &result.contract_markers_seen);
    if evaluation.should_force_fail() && status == TargetStatus::Pass {
        status = TargetStatus::Fail;
        error_message.clone_from(&evaluation.message);
    }

    let mut target_result = TargetResult::new(
        context.target.name.clone(),
        context.target.platform.clone(),
        status,
        "ssh",
    );
    target_result.duration_secs = Some(result.duration_secs);
    target_result.started_at = Some(context.started_at);
    target_result.completed_at = Some(result.completed_at);
    target_result.log_path = Some(context.log_path.display().to_string());
    target_result.phase = result.phase;
    target_result.last_output_at = result.last_output_at;
    target_result.last_heartbeat_at = result.last_heartbeat_at;
    target_result.error_message = error_message;
    target_result.contract_markers_seen = evaluation.seen;
    target_result.contract_markers_missing = evaluation.missing;
    target_result.contract_violation = evaluation.message;
    if status != TargetStatus::Pass {
        let stderr = if result.output.is_empty() {
            target_result.error_message.as_deref().unwrap_or_default()
        } else {
            &result.output
        };
        target_result.failure_class = Some(
            classify_failure(
                "",
                stderr,
                result.returncode,
                false,
                evaluation.violated && evaluation.enforce,
            )
            .as_str()
            .to_owned(),
        );
    }
    target_result
}

fn streaming_error_result(context: &SshRunContext<'_>, error: StreamingError) -> TargetResult {
    match error {
        StreamingError::Timeout { .. } => {
            let mut result = ssh_error_base(context);
            result.error_message = Some("Validation timed out".to_owned());
            result.failure_class = Some(FailureClass::Timeout.as_str().to_owned());
            result
        }
        StreamingError::Io(error) => ssh_error_result(context, &error.to_string()),
        StreamingError::MissingProgram => {
            ssh_error_result(context, "streaming command has no program")
        }
    }
}

fn ssh_error_result(context: &SshRunContext<'_>, message: &str) -> TargetResult {
    let mut result = ssh_error_base(context);
    result.error_message = Some(message.to_owned());
    result.failure_class = Some(
        classify_failure("", message, -1, false, false)
            .as_str()
            .to_owned(),
    );
    result
}

fn ssh_error_base(context: &SshRunContext<'_>) -> TargetResult {
    let mut result = TargetResult::new(
        context.target.name.clone(),
        context.target.platform.clone(),
        TargetStatus::Error,
        "ssh",
    );
    result.duration_secs = Some(context.start_time.elapsed().as_secs_f64());
    result.started_at = Some(context.started_at);
    result.completed_at = Some(Utc::now());
    result.log_path = Some(context.log_path.display().to_string());
    result
}

fn safe_filesize(path: &Path) -> i64 {
    path.metadata().map_or(-1, |metadata| {
        i64::try_from(metadata.len()).unwrap_or(i64::MAX)
    })
}

fn append_log(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .truncate(false)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.flush()
}

fn extract_ssh_error(output: &str) -> Option<String> {
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn build_staged_validation(
    sha: &str,
    stages: &BTreeMap<String, String>,
    resume_from: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    let mut skipping = resume_from.is_some();

    for stage in STAGE_ORDER {
        let Some(command) = stages.get(stage) else {
            continue;
        };
        if skipping {
            if Some(stage) == resume_from {
                skipping = false;
            } else {
                continue;
            }
        }
        let marker = stage_marker_name(stage, sha);
        parts.push(format!(
            "printf '__SHIPYARD_PHASE__:{stage}\\n' && {command} && touch {}",
            shlex_quote(&marker)
        ));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" && "))
    }
}

/// Build the remote marker filename written after a stage succeeds.
#[must_use]
pub fn stage_marker_name(stage: &str, sha: &str) -> String {
    let prefix_len = sha.len().min(12);
    format!(".shipyard-stage-{stage}-{}", &sha[..prefix_len])
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::time::Duration;

    use chrono::Utc;

    use crate::bundle::git_bundle::BundleResult;
    use crate::executor::contract::ContractConfig;
    use crate::executor::streaming::{
        StreamingCommand, StreamingCommandResult, StreamingCommandSpec, StreamingError,
    };
    use crate::job::TargetStatus;

    use super::{
        CommandCapture, ProbeCategory, ProbeDiagnostic, SshExecutor, SshOperations,
        SshTargetConfig, SshValidation, SshValidationRequest, build_probe_command,
        build_remote_command, classify_probe_error, diagnose_target_with, format_ssh_diagnosis,
        is_transient_ssh_error, shlex_quote, ssh_options,
    };

    struct FakeSshOperations {
        remote_has_sha: bool,
        remote_head: Option<String>,
        local_has_commit: bool,
        marker_exists: bool,
        upload_result: BundleResult,
        apply_result: BundleResult,
        create_results: RefCell<VecDeque<BundleResult>>,
        stream_results: RefCell<VecDeque<Result<StreamingCommandResult, StreamingError>>>,
        calls: RefCell<FakeCalls>,
    }

    #[derive(Default)]
    struct FakeCalls {
        create_basis: Vec<Vec<String>>,
        upload_count: usize,
        apply_count: usize,
        stream_argv: Vec<Vec<String>>,
        marker_names: Vec<String>,
        sleeps: Vec<Duration>,
    }

    impl FakeSshOperations {
        fn passing() -> Self {
            Self {
                remote_has_sha: true,
                remote_head: None,
                local_has_commit: false,
                marker_exists: false,
                upload_result: BundleResult::success("uploaded", "/tmp/shipyard.bundle"),
                apply_result: BundleResult::success("applied", "/tmp/shipyard.bundle"),
                create_results: RefCell::new(VecDeque::from([BundleResult::success(
                    "created",
                    "/tmp/shipyard.bundle",
                )])),
                stream_results: RefCell::new(VecDeque::from([Ok(stream_result(
                    0,
                    "ok\n",
                    Vec::new(),
                ))])),
                calls: RefCell::new(FakeCalls::default()),
            }
        }
    }

    impl SshOperations for FakeSshOperations {
        fn remote_has_sha(
            &self,
            _host: &str,
            _repo_path: &str,
            _sha: &str,
            _ssh_options: &[String],
        ) -> bool {
            self.remote_has_sha
        }

        fn remote_head_sha(
            &self,
            _host: &str,
            _repo_path: &str,
            _ssh_options: &[String],
        ) -> Option<String> {
            self.remote_head.clone()
        }

        fn local_has_commit(&self, _sha: &str, _repo_dir: Option<&Path>) -> bool {
            self.local_has_commit
        }

        fn create_bundle(
            &self,
            _sha: &str,
            _output_path: &Path,
            _repo_dir: Option<&Path>,
            basis_shas: &[String],
        ) -> BundleResult {
            self.calls
                .borrow_mut()
                .create_basis
                .push(basis_shas.to_vec());
            self.create_results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| BundleResult::success("created", "/tmp/shipyard.bundle"))
        }

        fn upload_bundle(
            &self,
            _bundle_path: &Path,
            _host: &str,
            _remote_path: &str,
            _ssh_options: &[String],
            _timeout: Duration,
        ) -> BundleResult {
            self.calls.borrow_mut().upload_count += 1;
            self.upload_result.clone()
        }

        fn apply_bundle(
            &self,
            _host: &str,
            _bundle_path: &str,
            _repo_path: &str,
            _ssh_options: &[String],
            _timeout: Duration,
        ) -> BundleResult {
            self.calls.borrow_mut().apply_count += 1;
            self.apply_result.clone()
        }

        fn remote_marker_exists(
            &self,
            _host: &str,
            _repo_path: &str,
            marker: &str,
            _ssh_options: &[String],
        ) -> bool {
            self.calls.borrow_mut().marker_names.push(marker.to_owned());
            self.marker_exists
        }

        fn run_streaming_command(
            &self,
            request: StreamingCommand<'_>,
        ) -> Result<StreamingCommandResult, StreamingError> {
            if let StreamingCommandSpec::Args(argv) = request.command {
                self.calls.borrow_mut().stream_argv.push(argv);
            }
            self.stream_results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(stream_result(0, "ok\n", Vec::new())))
        }

        fn sleep(&self, duration: Duration) {
            self.calls.borrow_mut().sleeps.push(duration);
        }
    }

    fn stream_result(
        returncode: i32,
        output: &str,
        contract_markers_seen: Vec<String>,
    ) -> StreamingCommandResult {
        let now = Utc::now();
        StreamingCommandResult {
            returncode,
            output: output.to_owned(),
            started_at: now,
            completed_at: now,
            duration_secs: 0.1,
            last_output_at: Some(now),
            phase: None,
            contract_markers_seen,
            last_heartbeat_at: Some(now),
            termination_reason: None,
        }
    }

    fn ssh_request(validation: SshValidation) -> SshValidationRequest<'static> {
        let mut target = SshTargetConfig {
            host: Some("builder.example".to_owned()),
            repo_path: "/remote/repo".to_owned(),
            ..SshTargetConfig::default()
        };
        target.ssh_options = vec!["-p".to_owned(), "2222".to_owned()];
        SshValidationRequest {
            sha: "feedfacecafebeef".to_owned(),
            branch: "main".to_owned(),
            target,
            validation,
            contract: None,
            log_path: tempfile::NamedTempFile::new()
                .expect("log")
                .path()
                .to_path_buf(),
            rotated_segments: 4,
            resume_from: None,
            mode: "default".to_owned(),
            progress_callback: None,
        }
    }

    fn stage_map(values: &[(&str, &str)]) -> BTreeMap<String, String> {
        values
            .iter()
            .map(|(stage, command)| ((*stage).to_owned(), (*command).to_owned()))
            .collect()
    }

    #[test]
    fn shlex_quote_matches_posix_contract() {
        assert_eq!(shlex_quote("abc123"), "abc123");
        assert_eq!(shlex_quote("/tmp/shipyard.bundle"), "/tmp/shipyard.bundle");
        assert_eq!(shlex_quote(""), "''");
        assert_eq!(shlex_quote("repo path"), "'repo path'");
        assert_eq!(shlex_quote("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn remote_command_builds_single_command() {
        let command = build_remote_command(
            "abc123",
            "/Users/me/repo path",
            &SshValidation::Command("make test".to_owned()),
            None,
        )
        .expect("command");

        assert_eq!(
            command,
            "cd '/Users/me/repo path' && git checkout --force abc123 && make test"
        );
    }

    #[test]
    fn remote_command_builds_stage_markers_in_order() {
        let mut stages = BTreeMap::new();
        stages.insert("test".to_owned(), "ctest --output-on-failure".to_owned());
        stages.insert("build".to_owned(), "cmake --build build".to_owned());
        let command = build_remote_command(
            "feedfacecafebeef",
            "~/repo",
            &SshValidation::Stages(stages),
            None,
        )
        .expect("command");

        assert!(command.contains("git checkout --force feedfacecafebeef"));
        let build_index = command.find("__SHIPYARD_PHASE__:build").expect("build");
        let test_index = command.find("__SHIPYARD_PHASE__:test").expect("test");
        assert!(build_index < test_index);
        assert!(command.contains("touch .shipyard-stage-build-feedfacecafe"));
        assert!(command.contains("touch .shipyard-stage-test-feedfacecafe"));
    }

    #[test]
    fn remote_command_honors_resume_from() {
        let mut stages = BTreeMap::new();
        stages.insert("setup".to_owned(), "./setup.sh".to_owned());
        stages.insert("build".to_owned(), "make".to_owned());
        stages.insert("test".to_owned(), "make test".to_owned());
        let command = build_remote_command(
            "feedfacecafebeef",
            "~/repo",
            &SshValidation::Stages(stages),
            Some("build"),
        )
        .expect("command");

        assert!(!command.contains("./setup.sh"));
        assert!(command.contains("make && touch"));
        assert!(command.contains("make test"));
    }

    #[test]
    fn remote_command_returns_none_when_no_stages_are_configured() {
        assert_eq!(
            build_remote_command(
                "abc123",
                "~/repo",
                &SshValidation::Stages(BTreeMap::new()),
                None,
            ),
            None
        );
    }

    #[test]
    fn probe_command_includes_batch_mode_and_connect_timeout() {
        let options = vec!["-i".to_owned(), "~/.ssh/id_ed25519".to_owned()];
        let remote = vec!["echo".to_owned(), "ok".to_owned()];
        assert_eq!(
            build_probe_command("host", &options, &remote),
            vec![
                "ssh",
                "-i",
                "~/.ssh/id_ed25519",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "BatchMode=yes",
                "host",
                "echo",
                "ok",
            ]
        );
    }

    #[test]
    fn ssh_options_appends_identity_file_after_configured_options() {
        assert_eq!(
            ssh_options(&["-p".to_owned(), "2222".to_owned()], Some("id_rsa")),
            vec!["-p", "2222", "-i", "id_rsa"]
        );
    }

    #[test]
    fn probe_error_classification_matches_python_buckets() {
        assert_eq!(
            classify_probe_error("Permission denied (publickey)", 255),
            ProbeCategory::Auth
        );
        assert_eq!(
            classify_probe_error("Host key verification failed", 255),
            ProbeCategory::HostKey
        );
        assert_eq!(
            classify_probe_error("Could not resolve hostname vm", 255),
            ProbeCategory::Resolution
        );
        assert_eq!(
            classify_probe_error("Connection refused", 255),
            ProbeCategory::Network
        );
        assert_eq!(
            classify_probe_error("Connection timed out", 255),
            ProbeCategory::Timeout
        );
        assert_eq!(classify_probe_error("", 255), ProbeCategory::Network);
        assert_eq!(classify_probe_error("weird", 1), ProbeCategory::Unknown);
    }

    #[test]
    fn diagnosis_formats_missing_host_hint() {
        let message = format_ssh_diagnosis(&ProbeDiagnostic {
            host: None,
            port: None,
            category: Some(ProbeCategory::Configuration),
            attempts: 0,
            last_error: Some("target has no host configured".to_owned()),
        });

        assert!(message.contains("SSH backend unreachable at <no host>."));
        assert!(message.contains("Failure category: configuration"));
        assert!(message.contains("cp -r <main-checkout>/.shipyard.local ./"));
    }

    #[test]
    fn diagnosis_formats_attempts_and_last_error() {
        let message = format_ssh_diagnosis(&ProbeDiagnostic {
            host: Some("vm.example".to_owned()),
            port: Some(2222),
            category: Some(ProbeCategory::Network),
            attempts: 3,
            last_error: Some("Connection refused".to_owned()),
        });

        assert!(message.contains("SSH backend unreachable at vm.example:2222."));
        assert!(message.contains("Attempts: 3"));
        assert!(message.contains("Last error: Connection refused"));
        assert!(!message.contains("cp -r <main-checkout>"));
    }

    #[test]
    fn transient_ssh_detection_matches_retry_contract() {
        assert!(is_transient_ssh_error("Connection reset by peer"));
        assert!(is_transient_ssh_error(
            "kex_exchange_identification: banner line"
        ));
        assert!(is_transient_ssh_error("Broken pipe"));
        assert!(!is_transient_ssh_error("Permission denied (publickey)"));
    }

    #[test]
    fn probe_diagnosis_fails_fast_for_missing_host() {
        let target = SshTargetConfig {
            host: None,
            ..SshTargetConfig::default()
        };

        let outcome = diagnose_target_with(
            &target,
            |_argv, _timeout| panic!("missing host must not spawn ssh"),
            |_| panic!("missing host must not sleep"),
        );

        assert!(!outcome.reachable);
        assert_eq!(
            outcome.diagnostic.category,
            Some(ProbeCategory::Configuration)
        );
        assert_eq!(outcome.diagnostic.attempts, 0);
    }

    #[test]
    fn probe_diagnosis_retries_transient_failures_with_ten_second_timeout() {
        let target = SshTargetConfig {
            host: Some("builder.example".to_owned()),
            ssh_options: vec!["-p".to_owned(), "2222".to_owned()],
            ..SshTargetConfig::default()
        };
        let mut attempts = 0;
        let mut timeouts = Vec::new();
        let mut backoffs = Vec::new();

        let outcome = diagnose_target_with(
            &target,
            |argv, timeout| {
                attempts += 1;
                timeouts.push(timeout);
                assert_eq!(argv[0], "ssh");
                assert!(argv.contains(&"ConnectTimeout=5".to_owned()));
                if attempts == 1 {
                    Ok(CommandCapture {
                        returncode: Some(255),
                        stdout: String::new(),
                        stderr: "Connection timed out".to_owned(),
                        timed_out: false,
                    })
                } else {
                    Ok(CommandCapture {
                        returncode: Some(0),
                        stdout: "ok\n".to_owned(),
                        stderr: String::new(),
                        timed_out: false,
                    })
                }
            },
            |backoff| backoffs.push(backoff),
        );

        assert!(outcome.reachable);
        assert_eq!(outcome.diagnostic.attempts, 2);
        assert_eq!(outcome.diagnostic.port, Some(2222));
        assert_eq!(
            timeouts,
            vec![Duration::from_secs(10), Duration::from_secs(10)]
        );
        assert_eq!(backoffs, vec![Duration::from_secs(2)]);
    }

    #[test]
    fn probe_diagnosis_does_not_retry_auth_failures() {
        let target = SshTargetConfig {
            host: Some("builder.example".to_owned()),
            ..SshTargetConfig::default()
        };
        let mut attempts = 0;

        let outcome = diagnose_target_with(
            &target,
            |_argv, _timeout| {
                attempts += 1;
                Ok(CommandCapture {
                    returncode: Some(255),
                    stdout: String::new(),
                    stderr: "Permission denied (publickey)".to_owned(),
                    timed_out: false,
                })
            },
            |_| panic!("auth failures are non-transient"),
        );

        assert!(!outcome.reachable);
        assert_eq!(attempts, 1);
        assert_eq!(outcome.diagnostic.category, Some(ProbeCategory::Auth));
        assert_eq!(
            outcome.diagnostic.last_error.as_deref(),
            Some("Permission denied (publickey)")
        );
    }

    #[test]
    fn executor_skips_bundle_when_remote_already_has_sha() {
        let operations = FakeSshOperations::passing();
        let executor = SshExecutor::with_operations(operations);

        let result = executor.validate(ssh_request(SshValidation::Command("make test".to_owned())));

        assert_eq!(result.status, TargetStatus::Pass);
        let calls = executor.operations.calls.borrow();
        assert!(calls.create_basis.is_empty());
        assert_eq!(calls.upload_count, 0);
        assert_eq!(calls.apply_count, 0);
        assert_eq!(calls.stream_argv.len(), 1);
        assert_eq!(calls.stream_argv[0][0], "ssh");
        assert!(calls.stream_argv[0].contains(&"-p".to_owned()));
        assert!(calls.stream_argv[0].contains(&"builder.example".to_owned()));
        assert!(
            calls
                .stream_argv
                .last()
                .and_then(|argv| argv.last())
                .expect("remote command")
                .contains("git checkout --force feedfacecafebeef && make test")
        );
    }

    #[test]
    fn executor_reports_missing_host_as_clean_error() {
        let operations = FakeSshOperations::passing();
        let executor = SshExecutor::with_operations(operations);
        let mut request = ssh_request(SshValidation::Command("make test".to_owned()));
        request.target.host = None;

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Error);
        assert!(
            result
                .error_message
                .expect("error")
                .contains("no `host` field")
        );
    }

    #[test]
    fn executor_uses_delta_bundle_when_remote_head_is_local_basis() {
        let mut operations = FakeSshOperations::passing();
        operations.remote_has_sha = false;
        operations.remote_head = Some("abc1234".to_owned());
        operations.local_has_commit = true;
        let executor = SshExecutor::with_operations(operations);
        let request = ssh_request(SshValidation::Command("make test".to_owned()));
        let log_path = request.log_path.clone();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Pass);
        let calls = executor.operations.calls.borrow();
        assert_eq!(calls.create_basis, vec![vec!["abc1234".to_owned()]]);
        assert_eq!(calls.upload_count, 1);
        assert_eq!(calls.apply_count, 1);
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("bundle_mode=delta"));
        assert!(log.contains("remote_head=abc1234"));
    }

    #[test]
    fn executor_falls_back_to_full_bundle_when_incremental_create_fails() {
        let mut operations = FakeSshOperations::passing();
        operations.remote_has_sha = false;
        operations.remote_head = Some("abc1234".to_owned());
        operations.local_has_commit = true;
        operations.create_results = RefCell::new(VecDeque::from([
            BundleResult::failure("incremental failed"),
            BundleResult::success("created", "/tmp/shipyard.bundle"),
        ]));
        let executor = SshExecutor::with_operations(operations);

        let result = executor.validate(ssh_request(SshValidation::Command("make test".to_owned())));

        assert_eq!(result.status, TargetStatus::Pass);
        assert_eq!(
            executor.operations.calls.borrow().create_basis,
            vec![vec!["abc1234".to_owned()], Vec::<String>::new()]
        );
    }

    #[test]
    fn executor_stops_before_validation_when_upload_fails() {
        let mut operations = FakeSshOperations::passing();
        operations.remote_has_sha = false;
        operations.upload_result = BundleResult::failure("upload broke");
        let executor = SshExecutor::with_operations(operations);

        let result = executor.validate(ssh_request(SshValidation::Command("make test".to_owned())));

        assert_eq!(result.status, TargetStatus::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("Bundle upload failed: upload broke")
        );
        assert!(executor.operations.calls.borrow().stream_argv.is_empty());
    }

    #[test]
    fn executor_stops_before_validation_when_apply_fails() {
        let mut operations = FakeSshOperations::passing();
        operations.remote_has_sha = false;
        operations.apply_result = BundleResult::failure("apply broke");
        let executor = SshExecutor::with_operations(operations);

        let result = executor.validate(ssh_request(SshValidation::Command("make test".to_owned())));

        assert_eq!(result.status, TargetStatus::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("Bundle apply failed: apply broke")
        );
        assert!(executor.operations.calls.borrow().stream_argv.is_empty());
    }

    #[test]
    fn executor_retries_transient_ssh_transport_errors() {
        let operations = FakeSshOperations {
            stream_results: RefCell::new(VecDeque::from([
                Ok(stream_result(255, "Connection reset by peer\n", Vec::new())),
                Ok(stream_result(0, "ok\n", Vec::new())),
            ])),
            ..FakeSshOperations::passing()
        };
        let executor = SshExecutor::with_operations(operations);

        let result = executor.validate(ssh_request(SshValidation::Command("make test".to_owned())));

        assert_eq!(result.status, TargetStatus::Pass);
        let calls = executor.operations.calls.borrow();
        assert_eq!(calls.stream_argv.len(), 2);
        assert_eq!(calls.sleeps, vec![Duration::from_secs(1)]);
    }

    #[test]
    fn executor_enforces_validation_contract_on_zero_exit() {
        let mut request = ssh_request(SshValidation::Command("make test".to_owned()));
        request.contract = Some(ContractConfig::new(vec!["SMOKE_DONE".to_owned()]));
        let executor = SshExecutor::with_operations(FakeSshOperations::passing());

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Fail);
        assert_eq!(result.failure_class.as_deref(), Some("CONTRACT"));
        assert_eq!(
            result.contract_markers_missing,
            vec!["SMOKE_DONE".to_owned()]
        );
    }

    #[test]
    fn executor_honors_resume_from_when_previous_stage_marker_exists() {
        let mut operations = FakeSshOperations::passing();
        operations.marker_exists = true;
        let executor = SshExecutor::with_operations(operations);
        let mut request = ssh_request(SshValidation::Stages(stage_map(&[
            ("setup", "./setup.sh"),
            ("build", "make"),
            ("test", "make test"),
        ])));
        request.resume_from = Some("build".to_owned());
        let log_path = request.log_path.clone();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Pass);
        let calls = executor.operations.calls.borrow();
        assert_eq!(
            calls.marker_names,
            vec![".shipyard-stage-setup-feedfacecafe".to_owned()]
        );
        let remote_command = calls
            .stream_argv
            .last()
            .and_then(|argv| argv.last())
            .expect("remote command");
        assert!(!remote_command.contains("./setup.sh"));
        assert!(remote_command.contains("__SHIPYARD_PHASE__:build"));
        assert!(remote_command.contains("__SHIPYARD_PHASE__:test"));
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("resume-from: honoring"));
    }

    #[test]
    fn executor_runs_all_stages_when_resume_marker_is_missing() {
        let executor = SshExecutor::with_operations(FakeSshOperations::passing());
        let mut request = ssh_request(SshValidation::Stages(stage_map(&[
            ("setup", "./setup.sh"),
            ("build", "make"),
        ])));
        request.resume_from = Some("build".to_owned());
        let log_path = request.log_path.clone();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Pass);
        let calls = executor.operations.calls.borrow();
        let remote_command = calls
            .stream_argv
            .last()
            .and_then(|argv| argv.last())
            .expect("remote command");
        assert!(remote_command.contains("./setup.sh"));
        assert!(remote_command.contains("__SHIPYARD_PHASE__:build"));
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("running all stages from the beginning"));
    }
}
