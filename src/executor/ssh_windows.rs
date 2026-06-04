//! Windows SSH PowerShell command contracts.
//!
//! The Python implementation wraps every remote Windows command in a
//! PowerShell script. This module keeps that wrapping logic isolated so
//! the Rust port can preserve behavior while the live executor is built
//! out around it.

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::{DateTime, Utc};
use wait_timeout::ChildExt;

use crate::bundle::git_bundle::{BundleResult, create_bundle, upload_bundle_windows};
use crate::classify::{FailureClass, classify_failure};
use crate::executor::clixml::maybe_decode_clixml;
use crate::executor::contract::{ContractConfig, evaluate_contract, required_markers};
use crate::executor::ssh::{ProbeCategory, ProbeDiagnostic, ProbeOutcome, classify_probe_error};
use crate::executor::streaming::{
    ProgressEvent, StreamingCommand, StreamingCommandResult, StreamingCommandSpec, StreamingError,
    run_streaming_command,
};
use crate::job::{TargetResult, TargetStatus};

/// Prelude prepended to every `PowerShell` command dispatched to Windows.
///
/// The three settings cover distinct encoding paths: `PowerShell`'s child
/// stdout decoding, `PowerShell`'s stdin encoding, and the Win32 console
/// code page seen by child processes. The settings are session-scoped.
pub const WINDOWS_UTF8_PRELUDE: &str = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
     $OutputEncoding = [System.Text.Encoding]::UTF8; \
     chcp.com 65001 | Out-Null; ";

const STAGE_ORDER: [&str; 4] = ["setup", "configure", "build", "test"];
const WINDOWS_PROBE_CONNECT_TIMEOUT_SECS: u64 = 5;
const WINDOWS_PROBE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const WINDOWS_PROBE_RETRY_BACKOFFS: [Duration; 2] =
    [Duration::from_secs(2), Duration::from_secs(6)];
const DEFAULT_TIMEOUT_SECS: u64 = 1_800;
const DEFAULT_BUNDLE_TIMEOUT_SECS: u64 = 1_800;
const DEFAULT_MUTEX_NAME: &str = r"Global\ShipyardValidate";

/// Validation script shape for a Windows target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WindowsValidation {
    /// A single validation command string.
    Command(String),
    /// Ordered validation stages keyed by Shipyard's stage names.
    Stages(BTreeMap<String, String>),
}

/// Visual Studio toolchain hints resolved on a Windows SSH host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VsToolchain {
    /// `CMake` platform value such as `ARM64` or `x64`.
    pub cmake_platform: String,
    /// Visual Studio installation path for `CMAKE_GENERATOR_INSTANCE`.
    pub cmake_generator_instance: String,
}

/// Target settings for a Windows SSH validation run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowsTargetConfig {
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
    /// Whether to run Visual Studio toolchain detection.
    pub windows_vs_detect: bool,
    /// Whether to serialize validation with a host-wide mutex.
    pub windows_host_mutex: bool,
    /// Optional host mutex name.
    pub windows_host_mutex_name: String,
}

impl Default for WindowsTargetConfig {
    fn default() -> Self {
        Self {
            name: "windows".to_owned(),
            platform: "windows-x64".to_owned(),
            host: None,
            repo_path: r"C:\repo".to_owned(),
            ssh_options: Vec::new(),
            identity_file: None,
            remote_bundle_path: "shipyard.bundle".to_owned(),
            local_repo_dir: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            bundle_upload_timeout_secs: DEFAULT_BUNDLE_TIMEOUT_SECS,
            bundle_apply_timeout_secs: DEFAULT_BUNDLE_TIMEOUT_SECS,
            windows_vs_detect: true,
            windows_host_mutex: true,
            windows_host_mutex_name: DEFAULT_MUTEX_NAME.to_owned(),
        }
    }
}

impl WindowsTargetConfig {
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

/// Request for one Windows SSH validation run.
pub struct WindowsValidationRequest<'a> {
    /// Commit SHA under validation.
    pub sha: String,
    /// Branch under validation.
    pub branch: String,
    /// Target execution settings.
    pub target: WindowsTargetConfig,
    /// Validation command/stage settings.
    pub validation: WindowsValidation,
    /// Optional validation contract.
    pub contract: Option<ContractConfig>,
    /// Local log file path.
    pub log_path: PathBuf,
    /// Optional stage to resume from.
    pub resume_from: Option<String>,
    /// Validation mode label.
    pub mode: String,
    /// Optional progress callback.
    pub progress_callback: Option<&'a mut dyn FnMut(ProgressEvent)>,
}

/// Result of checking the uploaded bundle on the remote Windows host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleProbe {
    /// Whether the remote path exists.
    pub exists: bool,
    /// Remote file size in bytes.
    pub size: u64,
    /// Human-readable probe detail for logs and errors.
    pub detail: String,
}

impl BundleProbe {
    fn present(size: u64, detail: impl Into<String>) -> Self {
        Self {
            exists: true,
            size,
            detail: detail.into(),
        }
    }

    fn missing(detail: impl Into<String>) -> Self {
        Self {
            exists: false,
            size: 0,
            detail: detail.into(),
        }
    }
}

impl WindowsValidationRequest<'_> {
    /// Create a request with Python-compatible defaults.
    #[must_use]
    pub fn new(log_path: PathBuf, validation: WindowsValidation) -> Self {
        Self {
            sha: String::new(),
            branch: String::new(),
            target: WindowsTargetConfig::default(),
            validation,
            contract: None,
            log_path,
            resume_from: None,
            mode: "default".to_owned(),
            progress_callback: None,
        }
    }
}

/// Errors returned while building `PowerShell` command strings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WindowsCommandError {
    /// The remote bundle path contains a single quote and cannot be
    /// safely embedded in the upload-side `PowerShell` script.
    QuotedRelativeBundlePath(String),
}

impl fmt::Display for WindowsCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuotedRelativeBundlePath(path) => {
                write!(formatter, "refusing single-quoted remote_path: {path:?}")
            }
        }
    }
}

impl std::error::Error for WindowsCommandError {}

/// Side-effect boundary used by the Windows SSH executor.
pub trait WindowsOperations {
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
        sha: &str,
        ssh_options: &[String],
    ) -> Option<String>;

    /// Create a bundle for delivery to the remote.
    fn create_bundle(
        &self,
        sha: &str,
        output_path: &Path,
        repo_dir: Option<&Path>,
        basis_shas: &[String],
    ) -> BundleResult;

    /// Upload a bundle to a Windows remote host.
    fn upload_bundle(
        &self,
        bundle_path: &Path,
        host: &str,
        remote_path: &str,
        ssh_options: &[String],
        timeout: Duration,
    ) -> BundleResult;

    /// Verify that a successful upload produced a non-empty remote bundle.
    fn probe_remote_bundle(
        &self,
        host: &str,
        bundle_path: &str,
        ssh_options: &[String],
        timeout: Duration,
    ) -> BundleProbe;

    /// Apply a bundle on a Windows remote host.
    fn apply_bundle(
        &self,
        host: &str,
        bundle_path: &str,
        repo_path: &str,
        ssh_options: &[String],
        timeout: Duration,
        log_file: &Path,
    ) -> BundleResult;

    /// Resolve Visual Studio toolchain hints for the host.
    fn detect_toolchain(
        &self,
        host: &str,
        ssh_options: &[String],
        target: &WindowsTargetConfig,
    ) -> Option<VsToolchain>;

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
}

/// Production Windows SSH operations backed by `ssh`, `git`, and git bundles.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemWindowsOperations;

impl WindowsOperations for SystemWindowsOperations {
    fn remote_has_sha(
        &self,
        host: &str,
        repo_path: &str,
        sha: &str,
        ssh_options: &[String],
    ) -> bool {
        let script = format!(
            "cd {}; git cat-file -e {}; exit $LASTEXITCODE",
            powershell_single_quoted(repo_path),
            powershell_single_quoted(sha)
        );
        run_capture(
            windows_ssh_command(host, ssh_options, &script),
            Duration::from_secs(15),
        )
        .is_ok_and(|output| output.success())
    }

    fn remote_head_sha(
        &self,
        host: &str,
        repo_path: &str,
        _sha: &str,
        ssh_options: &[String],
    ) -> Option<String> {
        let script = format!(
            "cd {}; git rev-parse HEAD",
            powershell_single_quoted(repo_path)
        );
        let output = run_capture(
            windows_ssh_command(host, ssh_options, &script),
            Duration::from_secs(15),
        )
        .ok()?;
        if !output.success() {
            return None;
        }
        let head = output.stdout.trim();
        looks_like_sha(head).then(|| head.to_owned())
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
        upload_bundle_windows(bundle_path, host, remote_path, ssh_options, timeout)
    }

    fn probe_remote_bundle(
        &self,
        host: &str,
        bundle_path: &str,
        ssh_options: &[String],
        timeout: Duration,
    ) -> BundleProbe {
        probe_remote_bundle(host, bundle_path, ssh_options, timeout)
    }

    fn apply_bundle(
        &self,
        host: &str,
        bundle_path: &str,
        repo_path: &str,
        ssh_options: &[String],
        timeout: Duration,
        log_file: &Path,
    ) -> BundleResult {
        apply_bundle_windows(host, bundle_path, repo_path, ssh_options, timeout, log_file)
    }

    fn detect_toolchain(
        &self,
        host: &str,
        ssh_options: &[String],
        target: &WindowsTargetConfig,
    ) -> Option<VsToolchain> {
        if !target.windows_vs_detect {
            return None;
        }
        detect_vs_toolchain(host, ssh_options)
    }

    fn remote_marker_exists(
        &self,
        host: &str,
        repo_path: &str,
        marker: &str,
        ssh_options: &[String],
    ) -> bool {
        let script = format!(
            "if (Test-Path (Join-Path {} {})) {{ exit 0 }} else {{ exit 1 }}",
            powershell_single_quoted(repo_path),
            powershell_single_quoted(marker)
        );
        run_capture(
            windows_ssh_command(host, ssh_options, &script),
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
}

/// Execute validation commands on Windows SSH targets.
#[derive(Clone, Debug)]
pub struct WindowsExecutor<O = SystemWindowsOperations> {
    operations: O,
}

impl Default for WindowsExecutor<SystemWindowsOperations> {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsExecutor<SystemWindowsOperations> {
    /// Construct a production Windows SSH executor.
    #[must_use]
    pub fn new() -> Self {
        Self::with_operations(SystemWindowsOperations)
    }
}

impl<O: WindowsOperations> WindowsExecutor<O> {
    /// Construct an executor using injected operations.
    #[must_use]
    pub fn with_operations(operations: O) -> Self {
        Self { operations }
    }

    /// Run a Windows SSH validation request and return a target result.
    #[must_use]
    pub fn validate(&self, mut request: WindowsValidationRequest<'_>) -> TargetResult {
        let progress_callback = request.progress_callback.take();
        self.validate_once(&request, progress_callback)
    }

    #[allow(clippy::too_many_lines)]
    fn validate_once(
        &self,
        request: &WindowsValidationRequest<'_>,
        progress_callback: Option<&mut dyn FnMut(ProgressEvent)>,
    ) -> TargetResult {
        let started_at = Utc::now();
        let start_time = Instant::now();
        let context = WindowsRunContext {
            target: &request.target,
            log_path: &request.log_path,
            started_at,
            start_time,
        };
        if let Some(parent) = request.log_path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            return windows_error_result(&context, &error.to_string());
        }

        let Some(host) = request
            .target
            .host
            .as_deref()
            .filter(|host| !host.trim().is_empty())
        else {
            return windows_error_result(
                &context,
                &format!(
                    "Target '{}' is misconfigured: no `host` field in .shipyard/config.toml or .shipyard.local/config.toml.",
                    request.target.name
                ),
            );
        };

        let ssh_options = windows_ssh_options(
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
                return windows_error_result(&context, &message);
            }
        }

        let toolchain = self
            .operations
            .detect_toolchain(host, &ssh_options, &request.target);
        let effective_resume = resolve_resume_from_windows(
            &WindowsResumeProbe {
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

        let Some(mut remote_command) = build_remote_command_with_toolchain(
            &request.sha,
            &request.target.repo_path,
            &request.validation,
            toolchain.as_ref(),
            effective_resume.as_deref(),
        ) else {
            return windows_error_result(&context, "No validation command configured");
        };

        if request.target.windows_host_mutex {
            remote_command = wrap_powershell_with_host_mutex(
                &remote_command,
                &request.target.windows_host_mutex_name,
            );
        }

        let mut argv = Vec::with_capacity(2 + ssh_options.len() + 8);
        argv.push("ssh".to_owned());
        argv.extend(ssh_options);
        argv.extend(powershell_encoded_argv(host, &remote_command));

        let mut stream_request = StreamingCommand::shell(String::new());
        stream_request.command = StreamingCommandSpec::Args(argv);
        stream_request.log_path = Some(request.log_path.clone());
        stream_request.timeout = Some(request.target.timeout());
        stream_request.required_contract_markers = required_markers(request.contract.as_ref());
        stream_request.progress_callback = progress_callback;

        match self.operations.run_streaming_command(stream_request) {
            Ok(result) => windows_command_result(&context, request.contract.as_ref(), result),
            Err(error) => windows_streaming_error_result(&context, error),
        }
    }

    fn deliver_bundle(
        &self,
        context: &WindowsRunContext<'_>,
        request: &WindowsValidationRequest<'_>,
        host: &str,
        ssh_options: &[String],
    ) -> Result<(), String> {
        let temp = tempfile::tempdir().map_err(|error| format!("OS error: {error}"))?;
        let bundle_path = temp.path().join("shipyard.bundle");
        let remote_head = self.operations.remote_head_sha(
            host,
            &request.target.repo_path,
            &request.sha,
            ssh_options,
        );
        let basis_shas = remote_head.iter().cloned().collect::<Vec<_>>();

        let mut bundle_result = self.operations.create_bundle(
            &request.sha,
            &bundle_path,
            request.target.local_repo_dir.as_deref(),
            &basis_shas,
        );
        if !bundle_result.success && !basis_shas.is_empty() {
            bundle_result = self.operations.create_bundle(
                &request.sha,
                &bundle_path,
                request.target.local_repo_dir.as_deref(),
                &[],
            );
        }
        if !bundle_result.success {
            return Err(format!("Bundle creation failed: {}", bundle_result.message));
        }

        let bundle_bytes = safe_filesize(&bundle_path);
        let expected_bundle_bytes = u64::try_from(bundle_bytes).ok();
        let _ =
            bootstrap_bundle_upload_log(context, request, host, &request.target.remote_bundle_path);
        let _ = append_log(
            context.log_path,
            &format!(
                "=== bundle_mode={} bundle_bytes={} sha={} remote_head={} ===\n",
                if basis_shas.is_empty() {
                    "full"
                } else {
                    "delta"
                },
                bundle_bytes,
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
            let _ = append_bundle_upload_failure_log(context.log_path, &upload_result);
            return Err(format!(
                "Bundle upload failed{}: {}",
                upload_failure_class_hint(&upload_result.failure_class),
                upload_result.message
            ));
        }

        self.ensure_uploaded_bundle_ready(
            context.log_path,
            host,
            &request.target.remote_bundle_path,
            ssh_options,
            expected_bundle_bytes,
        )?;

        let apply_result = self.operations.apply_bundle(
            host,
            &request.target.remote_bundle_path,
            &request.target.repo_path,
            ssh_options,
            request.target.bundle_apply_timeout(),
            context.log_path,
        );
        if !apply_result.success {
            return Err(format!("Bundle apply failed: {}", apply_result.message));
        }
        Ok(())
    }

    fn ensure_uploaded_bundle_ready(
        &self,
        log_path: &Path,
        host: &str,
        remote_bundle_path: &str,
        ssh_options: &[String],
        expected_bundle_bytes: Option<u64>,
    ) -> Result<(), String> {
        let probe = self.wait_for_uploaded_bundle(
            log_path,
            host,
            remote_bundle_path,
            ssh_options,
            expected_bundle_bytes,
        );
        let _ = append_log(
            log_path,
            &format!("bundle post-upload probe: {}\n", probe.detail),
        );
        if !probe.exists {
            return Err(format!(
                "Bundle upload completed but remote file is missing: {}. This is the failure mode from #247 (scp closed cleanly but the file isn't on the remote). Re-run should trigger a fresh upload.",
                probe.detail
            ));
        }
        if probe.size == 0 {
            return Err(format!(
                "Bundle upload completed but remote file is 0 bytes: {}. This is a silent truncation (#247). Re-run should trigger a fresh upload.",
                probe.detail
            ));
        }
        if let Some(expected) = expected_bundle_bytes
            && probe.size != expected
        {
            return Err(format!(
                "Bundle upload completed but remote file size is {} bytes; expected {expected} bytes: {}. This indicates an incomplete Windows upload; re-run should trigger a fresh upload.",
                probe.size, probe.detail
            ));
        }
        Ok(())
    }

    fn wait_for_uploaded_bundle(
        &self,
        log_path: &Path,
        host: &str,
        remote_bundle_path: &str,
        ssh_options: &[String],
        expected_bundle_bytes: Option<u64>,
    ) -> BundleProbe {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let probe = self.operations.probe_remote_bundle(
                host,
                remote_bundle_path,
                ssh_options,
                Duration::from_secs(30),
            );
            let size_matches = expected_bundle_bytes.is_none_or(|expected| probe.size == expected);
            if !probe.exists || probe.size == 0 || size_matches || Instant::now() >= deadline {
                return probe;
            }
            let _ = append_log(
                log_path,
                &format!(
                    "bundle post-upload probe: waiting for complete size; expected={expected_bundle_bytes:?} observed={} detail={}\n",
                    probe.size, probe.detail
                ),
            );
            sleep_bundle_probe_retry();
        }
    }
}

#[cfg(not(test))]
fn sleep_bundle_probe_retry() {
    std::thread::sleep(Duration::from_secs(2));
}

#[cfg(test)]
fn sleep_bundle_probe_retry() {}

/// Escape a value for the inside of a `PowerShell` single-quoted literal.
///
/// `PowerShell` escapes an embedded single quote by doubling it.
#[must_use]
pub fn escape_powershell_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

/// Wrap a value in a `PowerShell` single-quoted literal.
#[must_use]
pub fn powershell_single_quoted(value: &str) -> String {
    format!("'{}'", escape_powershell_single_quoted(value))
}

/// Encode a `PowerShell` script for `powershell -EncodedCommand`.
///
/// `PowerShell` expects base64 over UTF-16LE bytes. Sending multi-line
/// scripts this way avoids the Windows OpenSSH/cmd.exe newline-drop bug
/// that can otherwise produce false-green validations.
#[must_use]
pub fn encode_powershell_command(script: &str) -> String {
    let mut bytes = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode a `PowerShell` `-EncodedCommand` payload for tests/debugging.
#[must_use]
pub fn decode_powershell_command(encoded: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units).ok()
}

/// Build the `[host, powershell, ...]` tail for a Windows SSH argv.
#[must_use]
pub fn powershell_encoded_argv(host: &str, script: &str) -> Vec<String> {
    vec![
        host.to_owned(),
        "powershell".to_owned(),
        "-NoProfile".to_owned(),
        "-NonInteractive".to_owned(),
        "-OutputFormat".to_owned(),
        "Text".to_owned(),
        "-EncodedCommand".to_owned(),
        encode_powershell_command(script),
    ]
}

/// Decode the `-EncodedCommand` payload from an SSH argv.
#[must_use]
pub fn decode_encoded_ssh_argv(ssh_argv: &[String]) -> Option<String> {
    let index = ssh_argv.iter().position(|part| part == "-EncodedCommand")?;
    let encoded = ssh_argv.get(index + 1)?;
    decode_powershell_command(encoded)
}

/// Return true when `path` is absolute according to Shipyard's Windows
/// bundle/upload contract.
///
/// This intentionally treats drive-letter paths, slash-prefixed paths,
/// backslash-prefixed paths, and UNC paths as absolute. Upload and apply
/// must use the same predicate or bundle files can be written and read
/// from different locations.
#[must_use]
pub fn is_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if path.is_empty() {
        return false;
    }
    path.starts_with(r"\\")
        || path.starts_with('\\')
        || path.starts_with('/')
        || (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
}

/// Build the `PowerShell` expression that resolves a bundle path.
///
/// Absolute paths are quoted as-is. Relative paths resolve against
/// `$HOME` so upload and apply agree even when OpenSSH starts the shell
/// in a different working directory.
pub fn windows_bundle_path_expression(path: &str) -> Result<String, WindowsCommandError> {
    if path.contains('\'') {
        Err(WindowsCommandError::QuotedRelativeBundlePath(
            path.to_owned(),
        ))
    } else if is_windows_absolute_path(path) {
        Ok(powershell_single_quoted(path))
    } else {
        Ok(format!(
            "(Join-Path $HOME {})",
            powershell_single_quoted(path)
        ))
    }
}

/// Build the `PowerShell` script used to apply a previously uploaded git
/// bundle on a Windows host.
pub fn windows_apply_bundle_script(
    bundle_path: &str,
    repo_path: &str,
) -> Result<String, WindowsCommandError> {
    let resolved = windows_bundle_path_expression(bundle_path)?;
    let safe_repo = powershell_single_quoted(repo_path);
    Ok(format!(
        "{WINDOWS_UTF8_PRELUDE}\
         $Bundle = {resolved}; \
         if (-not (Test-Path -LiteralPath $Bundle)) {{ \
         Write-Error \"shipyard: bundle file not found at $Bundle \"\"\
         (expected after scp/ssh upload; check upload step logs)\"; \
         exit 1 }}; \
         cd {safe_repo}; \
         git bundle verify $Bundle; \
         if ($LASTEXITCODE -ne 0) {{ exit 1 }}; \
         git fetch $Bundle \
         '+refs/heads/*:refs/shipyard-bundles/heads/*' \
         '+refs/tags/*:refs/shipyard-bundles/tags/*'; \
         if ($LASTEXITCODE -ne 0) {{ exit 1 }}"
    ))
}

/// `PowerShell` snippet that exports the resolved Visual Studio toolchain.
#[must_use]
pub fn toolchain_env_exports(toolchain: Option<&VsToolchain>) -> String {
    let Some(toolchain) = toolchain else {
        return "$env:SHIPYARD_CMAKE_PLATFORM = ''; $env:SHIPYARD_CMAKE_GENERATOR_INSTANCE = ''"
            .to_owned();
    };
    format!(
        "$env:SHIPYARD_CMAKE_PLATFORM = {}; $env:SHIPYARD_CMAKE_GENERATOR_INSTANCE = {}",
        powershell_single_quoted(&toolchain.cmake_platform),
        powershell_single_quoted(&toolchain.cmake_generator_instance)
    )
}

/// Wrap a `PowerShell` validation body in a host-wide mutex.
#[must_use]
pub fn wrap_powershell_with_host_mutex(ps_body: &str, mutex_name: &str) -> String {
    let safe_mutex = escape_powershell_single_quoted(mutex_name);
    format!(
        r#"$ErrorActionPreference = 'Stop'
$__ShipyardExit = 1
$MutexName = '{safe_mutex}'
$Mutex = New-Object System.Threading.Mutex($false, $MutexName)
$LockAcquired = $false
try {{
    try {{
        if ($Mutex.WaitOne(0)) {{
            $LockAcquired = $true
        }} else {{
            Write-Host "__SHIPYARD_WAIT__:host-lock"
            Write-Host "__SHIPYARD_PHASE__:waiting-lock"
            Write-Host "Waiting for host validation lock: $MutexName"
            $null = $Mutex.WaitOne()
            $LockAcquired = $true
        }}
    }} catch [System.Threading.AbandonedMutexException] {{
        Write-Host "Recovered abandoned host validation lock: $MutexName"
        $LockAcquired = $true
    }}

    try {{
        {ps_body}
        $__ShipyardExit = $LASTEXITCODE
        if ($null -eq $__ShipyardExit) {{
            $__ShipyardExit = 0
        }}
    }} catch {{
        Write-Error ("Shipyard body raised: " + $_.Exception.Message)
        Write-Error ($_.ScriptStackTrace)
        $__ShipyardExit = 1
    }}
}} finally {{
    if ($LockAcquired) {{
        try {{
            $Mutex.ReleaseMutex() | Out-Null
        }} catch [System.ApplicationException] {{
        }}
    }}
    $Mutex.Dispose()
}}
exit $__ShipyardExit"#,
    )
}

/// Build the `PowerShell` validation command run on a Windows host.
///
/// Returns `None` when a staged validation config has no active stages.
#[must_use]
pub fn build_remote_command(
    sha: &str,
    remote_repo: &str,
    validation: &WindowsValidation,
    resume_from: Option<&str>,
) -> Option<String> {
    build_remote_command_with_toolchain(sha, remote_repo, validation, None, resume_from)
}

/// Build the remote `PowerShell` validation command with optional toolchain hints.
#[must_use]
pub fn build_remote_command_with_toolchain(
    sha: &str,
    remote_repo: &str,
    validation: &WindowsValidation,
    toolchain: Option<&VsToolchain>,
    resume_from: Option<&str>,
) -> Option<String> {
    let validate_cmd = match validation {
        WindowsValidation::Command(command) => command.clone(),
        WindowsValidation::Stages(stages) => build_staged_validation(sha, stages, resume_from)?,
    };

    Some(format!(
        "{WINDOWS_UTF8_PRELUDE}\
         {}; \
         cd {}; \
         git checkout --force {}; \
         if ($LASTEXITCODE -ne 0) {{ exit 1 }}; \
         {validate_cmd}",
        toolchain_env_exports(toolchain),
        powershell_single_quoted(remote_repo),
        powershell_single_quoted(sha),
    ))
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

fn windows_ssh_command(host: &str, ssh_options: &[String], script: &str) -> Command {
    let mut command = Command::new("ssh");
    command.args(ssh_options);
    command.args(powershell_encoded_argv(host, script));
    command
}

/// Build the `ssh` argv for a Windows reachability probe.
///
/// The probe deliberately uses plain `echo ok`, matching Python Shipyard.
/// That command works under Windows OpenSSH's default `cmd.exe`, `PowerShell`,
/// and POSIX shells, and avoids false negatives from slow `PowerShell` startup.
#[must_use]
pub fn build_windows_probe_command(host: &str, ssh_options: &[String]) -> Vec<String> {
    let mut command = Vec::with_capacity(12 + ssh_options.len());
    command.push("ssh".to_owned());
    command.extend_from_slice(ssh_options);
    command.push("-o".to_owned());
    command.push(format!(
        "ConnectTimeout={WINDOWS_PROBE_CONNECT_TIMEOUT_SECS}"
    ));
    command.push("-o".to_owned());
    command.push("BatchMode=yes".to_owned());
    command.push(host.to_owned());
    command.push("echo".to_owned());
    command.push("ok".to_owned());
    command
}

/// Probe one Windows SSH target for preflight reachability.
#[must_use]
pub fn probe_target(target: &WindowsTargetConfig) -> bool {
    diagnose_target(target).reachable
}

/// Diagnose one Windows SSH target for preflight reachability.
#[must_use]
pub fn diagnose_target(target: &WindowsTargetConfig) -> ProbeOutcome {
    diagnose_target_with(target, run_probe_argv, std::thread::sleep)
}

fn diagnose_target_with<R, S>(
    target: &WindowsTargetConfig,
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

    let ssh_options = windows_ssh_options(&target.ssh_options, target.identity_file.as_deref());
    let argv = build_windows_probe_command(host, &ssh_options);
    let mut attempts = 0;

    loop {
        attempts += 1;
        let (last_error, last_category) = match runner(&argv, WINDOWS_PROBE_ATTEMPT_TIMEOUT) {
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
                    && let Some(backoff) = WINDOWS_PROBE_RETRY_BACKOFFS.get(attempts - 1)
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
        return maybe_decode_clixml(stderr);
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

fn windows_ssh_options(configured_options: &[String], identity_file: Option<&str>) -> Vec<String> {
    let mut options = configured_options.to_vec();
    if let Some(identity_file) = identity_file {
        options.extend(["-i".to_owned(), identity_file.to_owned()]);
    }
    options
}

fn looks_like_sha(value: &str) -> bool {
    value.len() >= 7 && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn apply_bundle_windows(
    host: &str,
    bundle_path: &str,
    repo_path: &str,
    ssh_options: &[String],
    timeout: Duration,
    log_file: &Path,
) -> BundleResult {
    let script = match windows_apply_bundle_script(bundle_path, repo_path) {
        Ok(script) => script,
        Err(error) => return BundleResult::failure(error.to_string()),
    };
    match run_capture(windows_ssh_command(host, ssh_options, &script), timeout) {
        Ok(output) if output.timed_out => BundleResult::failure("Remote bundle apply timed out"),
        Ok(output) if output.success() => BundleResult::success("Bundle applied", bundle_path),
        Ok(output) => {
            let stderr_log_path = write_bundle_apply_stderr(log_file, &output);
            let detail = maybe_decode_clixml(output.stderr.trim());
            let mut message = format!("Remote bundle apply failed: {detail}");
            if let Some(path) = stderr_log_path {
                let _ = write!(message, " (raw stderr: {})", path.display());
            }
            BundleResult::failure(message)
        }
        Err(error) => BundleResult::failure(format!("OS error: {error}")),
    }
}

fn probe_remote_bundle(
    host: &str,
    bundle_path: &str,
    ssh_options: &[String],
    timeout: Duration,
) -> BundleProbe {
    let bundle_expression = match windows_bundle_path_expression(bundle_path) {
        Ok(expression) => expression,
        Err(error) => return BundleProbe::missing(format!("probe error: {error}")),
    };
    let script = format!(
        "{WINDOWS_UTF8_PRELUDE}$Bundle = {bundle_expression}; \
         if (Test-Path -LiteralPath $Bundle) {{ \
         $i = Get-Item -LiteralPath $Bundle; \
         Write-Output (\"OK size=\" + $i.Length + \" mtime=\" + $i.LastWriteTimeUtc.ToString('o') + \" path=\" + $Bundle) \
         }} else {{ \
         Write-Output (\"MISSING path=\" + $Bundle) \
         }}"
    );
    match run_capture(windows_ssh_command(host, ssh_options, &script), timeout) {
        Ok(output) if output.timed_out => {
            BundleProbe::missing(format!("probe timed out after {}s", timeout.as_secs()))
        }
        Ok(output) if !output.success() => {
            let stderr = output.stderr.trim().chars().take(200).collect::<String>();
            let suffix = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            };
            BundleProbe::missing(format!(
                "probe exited {}{suffix}",
                output.returncode.unwrap_or(-1)
            ))
        }
        Ok(output) => parse_bundle_probe_stdout(&output.stdout),
        Err(error) => BundleProbe::missing(format!("probe error: {error}")),
    }
}

fn parse_bundle_probe_stdout(stdout: &str) -> BundleProbe {
    if let Some(line) = sentinel_line(stdout, "OK ")
        && let Some(probe) = parse_ok_bundle_probe(line)
    {
        return probe;
    }
    if let Some(line) = sentinel_line(stdout, "MISSING ")
        && let Some(path) = value_after(line, "path=")
    {
        return BundleProbe::missing(format!("MISSING path={}", path.trim()));
    }
    BundleProbe::missing(format!(
        "probe unexpected output: {:?}",
        stdout.trim().chars().take(200).collect::<String>()
    ))
}

fn sentinel_line<'a>(stdout: &'a str, marker: &str) -> Option<&'a str> {
    let start = stdout.find(marker)?;
    Some(stdout[start..].lines().next().unwrap_or_default().trim())
}

fn parse_ok_bundle_probe(line: &str) -> Option<BundleProbe> {
    let size = value_between(line, "size=", " ")?.parse::<u64>().ok()?;
    let mtime = value_between(line, "mtime=", " ")?;
    let path = value_after(line, "path=")?.trim();
    Some(BundleProbe::present(
        size,
        format!("OK size={size} mtime={mtime} path={path}"),
    ))
}

fn value_between<'a>(line: &'a str, key: &str, delimiter: &str) -> Option<&'a str> {
    let rest = value_after(line, key)?;
    let end = rest.find(delimiter)?;
    Some(&rest[..end])
}

fn value_after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let start = line.find(key)? + key.len();
    Some(&line[start..])
}

fn write_bundle_apply_stderr(log_file: &Path, output: &CommandCapture) -> Option<PathBuf> {
    let path = PathBuf::from(format!("{}.bundle-apply-stderr", log_file.display()));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok()?;
    }
    fs::write(
        &path,
        format!(
            "=== exit_code={} ===\n=== stderr (bytes={}) ===\n{}\n=== stdout (bytes={}) ===\n{}\n",
            output.returncode.unwrap_or(-1),
            output.stderr.len(),
            output.stderr,
            output.stdout.len(),
            output.stdout
        ),
    )
    .ok()?;
    Some(path)
}

fn detect_vs_toolchain(host: &str, ssh_options: &[String]) -> Option<VsToolchain> {
    let output = run_capture(
        windows_ssh_command(host, ssh_options, VS_DETECT_SCRIPT),
        Duration::from_mins(1),
    )
    .ok()?;
    if !output.success() {
        return None;
    }
    output.stdout.lines().rev().find_map(parse_toolchain_line)
}

fn parse_toolchain_line(line: &str) -> Option<VsToolchain> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let data: serde_json::Value = serde_json::from_str(line).ok()?;
    let platform = data
        .get("platform")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    let instance = data
        .get("generator_instance")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    if platform.is_empty() && instance.is_empty() {
        return None;
    }
    Some(VsToolchain {
        cmake_platform: platform.to_owned(),
        cmake_generator_instance: instance.to_owned(),
    })
}

const VS_DETECT_SCRIPT: &str = r#"
function Resolve-CMakePlatform {
    if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
        return 'ARM64'
    }
    return 'x64'
}

function Resolve-VisualStudioInstance {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) {
        return ''
    }
    try {
        $raw = (& $vswhere -products * -format json) -join "`n"
        if (-not $raw) {
            return ''
        }
        $instances = $raw | ConvertFrom-Json
        if ($instances -isnot [System.Array]) {
            $instances = @($instances)
        }
        $instances = $instances | Sort-Object -Property installDate -Descending
        $preferred = $instances | Where-Object {
            $_.productId -and $_.productId -ne 'Microsoft.VisualStudio.Product.BuildTools'
        } | Select-Object -First 1
        if (-not $preferred) {
            $preferred = $instances | Select-Object -First 1
        }
        if ($preferred -and $preferred.installationPath) {
            return $preferred.installationPath.Replace('\', '/')
        }
    } catch {
    }
    return ''
}

$resolved = @{
    platform = Resolve-CMakePlatform
    generator_instance = Resolve-VisualStudioInstance
}
$resolved | ConvertTo-Json -Compress
"#;

struct WindowsResumeProbe<'a, O> {
    operations: &'a O,
    host: &'a str,
    remote_repo: &'a str,
    sha: &'a str,
    ssh_options: &'a [String],
    validation: &'a WindowsValidation,
    log_file: &'a Path,
}

fn resolve_resume_from_windows<O: WindowsOperations>(
    probe: &WindowsResumeProbe<'_, O>,
    requested: Option<&str>,
) -> Option<String> {
    let requested = requested?;
    let WindowsValidation::Stages(stages) = probe.validation else {
        let _ = append_log(
            probe.log_file,
            &format!(
                "=== resume-from: ignored ({requested:?}) - validation_config uses single command ===\n"
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

struct WindowsRunContext<'a> {
    target: &'a WindowsTargetConfig,
    log_path: &'a Path,
    started_at: DateTime<Utc>,
    start_time: Instant,
}

fn windows_command_result(
    context: &WindowsRunContext<'_>,
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
            extract_windows_ssh_error(&result.output)
                .unwrap_or_else(|| "SSH transport failed".to_owned()),
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
        "ssh-windows",
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

fn windows_streaming_error_result(
    context: &WindowsRunContext<'_>,
    error: StreamingError,
) -> TargetResult {
    match error {
        StreamingError::Timeout { .. } => {
            let mut result = windows_error_base(context);
            result.error_message = Some("Validation timed out".to_owned());
            result.failure_class = Some(FailureClass::Timeout.as_str().to_owned());
            result
        }
        StreamingError::Io(error) => windows_error_result(context, &error.to_string()),
        StreamingError::MissingProgram => {
            windows_error_result(context, "streaming command has no program")
        }
    }
}

fn windows_error_result(context: &WindowsRunContext<'_>, message: &str) -> TargetResult {
    let mut result = windows_error_base(context);
    result.error_message = Some(message.to_owned());
    result.failure_class = Some(
        classify_failure("", message, -1, false, false)
            .as_str()
            .to_owned(),
    );
    result
}

fn windows_error_base(context: &WindowsRunContext<'_>) -> TargetResult {
    let mut result = TargetResult::new(
        context.target.name.clone(),
        context.target.platform.clone(),
        TargetStatus::Error,
        "ssh-windows",
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

fn bootstrap_bundle_upload_log(
    context: &WindowsRunContext<'_>,
    request: &WindowsValidationRequest<'_>,
    host: &str,
    remote_bundle: &str,
) -> std::io::Result<()> {
    if let Some(parent) = context.log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    let _ = writeln!(text, "# shipyard ssh-windows lane log");
    let _ = writeln!(text, "target: {}", context.target.name);
    let _ = writeln!(text, "platform: {}", context.target.platform);
    let _ = writeln!(text, "host: {host}");
    let _ = writeln!(text, "sha: {}", request.sha);
    let _ = writeln!(text, "branch: {}", request.branch);
    let _ = writeln!(text, "remote_repo: {}", context.target.repo_path);
    let _ = writeln!(text, "remote_bundle: {remote_bundle}");
    let _ = writeln!(text, "started_at: {}", context.started_at.to_rfc3339());
    let _ = writeln!(text, "---");
    fs::write(context.log_path, text)
}

fn append_bundle_upload_failure_log(
    path: &Path,
    upload_result: &BundleResult,
) -> std::io::Result<()> {
    let mut text = format!(
        "bundle-upload failure (class={})\n",
        upload_result.failure_class
    );
    for attempt in &upload_result.attempts {
        let _ = writeln!(text, "  {attempt}");
    }
    let _ = writeln!(text, "summary: {}", upload_result.message);
    append_log(path, &text)
}

fn upload_failure_class_hint(failure_class: &str) -> &'static str {
    match failure_class {
        "ssh_unreachable" => " [ssh-unreachable]",
        "upload_failed" => " [upload failed after reachable]",
        _ => "",
    }
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

fn extract_windows_ssh_error(output: &str) -> Option<String> {
    let decoded = maybe_decode_clixml(output);
    decoded
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
        let marker = powershell_single_quoted(&stage_marker_name(stage, sha));
        parts.push(format!(
            "Write-Output '__SHIPYARD_PHASE__:{stage}'; {command}; \
             if ($LASTEXITCODE -ne 0) {{ exit 1 }}; \
             New-Item -ItemType File -Force -Path {marker} | Out-Null"
        ));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; if ($LASTEXITCODE -ne 0) { exit 1 }; "))
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
    use crate::executor::ssh::ProbeCategory;
    use crate::executor::streaming::{
        StreamingCommand, StreamingCommandResult, StreamingCommandSpec, StreamingError,
    };
    use crate::job::TargetStatus;

    use super::{
        BundleProbe, CommandCapture, WINDOWS_UTF8_PRELUDE, WindowsCommandError, WindowsExecutor,
        WindowsOperations, WindowsTargetConfig, WindowsValidation, WindowsValidationRequest,
        build_remote_command, build_remote_command_with_toolchain, build_windows_probe_command,
        decode_encoded_ssh_argv, decode_powershell_command, diagnose_target_with,
        encode_powershell_command, escape_powershell_single_quoted, is_windows_absolute_path,
        parse_bundle_probe_stdout, powershell_encoded_argv, powershell_single_quoted,
        toolchain_env_exports, windows_apply_bundle_script, windows_bundle_path_expression,
        wrap_powershell_with_host_mutex,
    };

    struct FakeWindowsOperations {
        remote_has_sha: bool,
        remote_head: Option<String>,
        marker_exists: bool,
        toolchain: Option<super::VsToolchain>,
        upload_result: BundleResult,
        probe_result: BundleProbe,
        probe_results: RefCell<VecDeque<BundleProbe>>,
        apply_result: BundleResult,
        created_bundle_bytes: Option<usize>,
        create_results: RefCell<VecDeque<BundleResult>>,
        stream_results: RefCell<VecDeque<Result<StreamingCommandResult, StreamingError>>>,
        calls: RefCell<FakeCalls>,
    }

    #[derive(Default)]
    struct FakeCalls {
        create_basis: Vec<Vec<String>>,
        upload_count: usize,
        probe_count: usize,
        apply_count: usize,
        stream_argv: Vec<Vec<String>>,
        marker_names: Vec<String>,
        toolchain_count: usize,
    }

    impl FakeWindowsOperations {
        fn passing() -> Self {
            Self {
                remote_has_sha: true,
                remote_head: None,
                marker_exists: false,
                toolchain: None,
                upload_result: BundleResult::success("uploaded", "shipyard.bundle"),
                probe_result: BundleProbe::present(
                    123,
                    "OK size=123 mtime=2026-04-24T20:00:00Z path=shipyard.bundle",
                ),
                probe_results: RefCell::new(VecDeque::new()),
                apply_result: BundleResult::success("applied", "shipyard.bundle"),
                created_bundle_bytes: None,
                create_results: RefCell::new(VecDeque::from([BundleResult::success(
                    "created",
                    "shipyard.bundle",
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

    impl WindowsOperations for FakeWindowsOperations {
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
            _sha: &str,
            _ssh_options: &[String],
        ) -> Option<String> {
            self.remote_head.clone()
        }

        fn create_bundle(
            &self,
            _sha: &str,
            output_path: &Path,
            _repo_dir: Option<&Path>,
            basis_shas: &[String],
        ) -> BundleResult {
            self.calls
                .borrow_mut()
                .create_basis
                .push(basis_shas.to_vec());
            if let Some(bytes) = self.created_bundle_bytes {
                std::fs::write(output_path, vec![b'x'; bytes]).expect("write fake bundle");
            }
            self.create_results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| BundleResult::success("created", "shipyard.bundle"))
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

        fn probe_remote_bundle(
            &self,
            _host: &str,
            _bundle_path: &str,
            _ssh_options: &[String],
            _timeout: Duration,
        ) -> BundleProbe {
            self.calls.borrow_mut().probe_count += 1;
            self.probe_results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| self.probe_result.clone())
        }

        fn apply_bundle(
            &self,
            _host: &str,
            _bundle_path: &str,
            _repo_path: &str,
            _ssh_options: &[String],
            _timeout: Duration,
            _log_file: &Path,
        ) -> BundleResult {
            self.calls.borrow_mut().apply_count += 1;
            self.apply_result.clone()
        }

        fn detect_toolchain(
            &self,
            _host: &str,
            _ssh_options: &[String],
            _target: &WindowsTargetConfig,
        ) -> Option<super::VsToolchain> {
            self.calls.borrow_mut().toolchain_count += 1;
            self.toolchain.clone()
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

    fn windows_request(validation: WindowsValidation) -> WindowsValidationRequest<'static> {
        let mut target = WindowsTargetConfig {
            host: Some("windows.example".to_owned()),
            repo_path: r"C:\repo".to_owned(),
            ..WindowsTargetConfig::default()
        };
        target.ssh_options = vec!["-p".to_owned(), "2222".to_owned()];
        WindowsValidationRequest {
            sha: "feedfacecafebeef".to_owned(),
            branch: "main".to_owned(),
            target,
            validation,
            contract: None,
            log_path: tempfile::NamedTempFile::new()
                .expect("log")
                .path()
                .to_path_buf(),
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
    fn prelude_contains_all_three_encoding_settings() {
        assert!(WINDOWS_UTF8_PRELUDE.contains("chcp.com 65001"));
        assert!(WINDOWS_UTF8_PRELUDE.contains("[Console]::OutputEncoding"));
        assert!(WINDOWS_UTF8_PRELUDE.contains("$OutputEncoding"));
        assert!(WINDOWS_UTF8_PRELUDE.contains("| Out-Null"));
    }

    #[test]
    fn prelude_order_sets_encodings_before_chcp() {
        let console = WINDOWS_UTF8_PRELUDE
            .find("[Console]::OutputEncoding")
            .expect("console setter");
        let output = WINDOWS_UTF8_PRELUDE
            .find("$OutputEncoding")
            .expect("output setter");
        let chcp = WINDOWS_UTF8_PRELUDE.find("chcp.com").expect("chcp");
        assert!(console < chcp);
        assert!(output < chcp);
    }

    #[test]
    fn powershell_single_quote_doubles_embedded_quotes() {
        assert_eq!(escape_powershell_single_quoted("it's"), "it''s");
        assert_eq!(powershell_single_quoted("C:/repo's"), "'C:/repo''s'");
    }

    #[test]
    fn encoded_powershell_argv_round_trips_utf16le_payload() {
        let script = "Write-Output 'hello — windows'";
        let encoded = encode_powershell_command(script);

        assert_eq!(decode_powershell_command(&encoded).as_deref(), Some(script));
        let argv = powershell_encoded_argv("host", script);
        assert_eq!(argv[0], "host");
        assert!(argv.contains(&"-NoProfile".to_owned()));
        assert!(argv.contains(&"-NonInteractive".to_owned()));
        assert_eq!(decode_encoded_ssh_argv(&argv).as_deref(), Some(script));
    }

    #[test]
    fn windows_probe_command_uses_batch_mode_timeout_and_shell_portable_echo() {
        let argv = build_windows_probe_command("win-host", &["-p".to_owned(), "2222".to_owned()]);

        assert_eq!(argv[0], "ssh");
        assert!(argv.contains(&"ConnectTimeout=5".to_owned()));
        assert!(argv.contains(&"BatchMode=yes".to_owned()));
        assert_eq!(
            argv[argv.len() - 3..],
            ["win-host".to_owned(), "echo".to_owned(), "ok".to_owned()]
        );
        assert!(decode_encoded_ssh_argv(&argv).is_none());
    }

    #[test]
    fn windows_probe_diagnosis_fails_fast_for_missing_host() {
        let target = WindowsTargetConfig {
            host: None,
            ..WindowsTargetConfig::default()
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
    fn windows_probe_diagnosis_retries_transient_failures() {
        let target = WindowsTargetConfig {
            host: Some("win-host".to_owned()),
            ssh_options: vec!["-p".to_owned(), "2222".to_owned()],
            ..WindowsTargetConfig::default()
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
                if attempts == 1 {
                    Ok(CommandCapture {
                        returncode: Some(255),
                        stdout: String::new(),
                        stderr: "Connection refused".to_owned(),
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
    fn toolchain_exports_and_mutex_wrapper_match_python_contracts() {
        let toolchain = super::VsToolchain {
            cmake_platform: "ARM64".to_owned(),
            cmake_generator_instance: "C:/VS/Preview".to_owned(),
        };
        let exports = toolchain_env_exports(Some(&toolchain));
        assert!(exports.contains("$env:SHIPYARD_CMAKE_PLATFORM = 'ARM64'"));
        assert!(exports.contains("$env:SHIPYARD_CMAKE_GENERATOR_INSTANCE = 'C:/VS/Preview'"));
        assert!(toolchain_env_exports(None).contains("$env:SHIPYARD_CMAKE_PLATFORM = ''"));

        let wrapped = wrap_powershell_with_host_mutex("ctest", r"Global\ShipyardValidate");
        assert!(wrapped.contains("__SHIPYARD_WAIT__:host-lock"));
        assert!(wrapped.contains("__SHIPYARD_PHASE__:waiting-lock"));
        assert!(wrapped.contains("exit $__ShipyardExit"));
    }

    #[test]
    fn absolute_path_predicate_matches_windows_upload_apply_contract() {
        for path in [
            r"C:\foo\bar",
            "C:/foo/bar",
            r"c:\foo",
            "/tmp/x.bundle",
            r"\foo",
            r"\\server\share\file",
        ] {
            assert!(is_windows_absolute_path(path), "{path}");
        }

        for path in [
            "",
            "shipyard.bundle",
            r"sub\file",
            "sub/file",
            "1:/not-drive",
        ] {
            assert!(!is_windows_absolute_path(path), "{path}");
        }
    }

    #[test]
    fn bundle_expression_resolves_relative_paths_against_home() {
        assert_eq!(
            windows_bundle_path_expression("shipyard.bundle").expect("path"),
            "(Join-Path $HOME 'shipyard.bundle')"
        );
    }

    #[test]
    fn bundle_expression_keeps_absolute_paths_as_is() {
        assert_eq!(
            windows_bundle_path_expression(r"C:\shipyard.bundle").expect("path"),
            r"'C:\shipyard.bundle'"
        );
        assert_eq!(
            windows_bundle_path_expression("/tmp/shipyard.bundle").expect("path"),
            "'/tmp/shipyard.bundle'"
        );
        assert_eq!(
            windows_bundle_path_expression(r"\\server\share\shipyard.bundle").expect("path"),
            r"'\\server\share\shipyard.bundle'"
        );
    }

    #[test]
    fn bundle_expression_rejects_quoted_relative_paths() {
        assert_eq!(
            windows_bundle_path_expression("ship'yard.bundle"),
            Err(WindowsCommandError::QuotedRelativeBundlePath(
                "ship'yard.bundle".to_owned()
            ))
        );
    }

    #[test]
    fn bundle_expression_rejects_quoted_rooted_paths() {
        assert_eq!(
            windows_bundle_path_expression("/tmp/o'hare.bundle"),
            Err(WindowsCommandError::QuotedRelativeBundlePath(
                "/tmp/o'hare.bundle".to_owned()
            ))
        );
        assert_eq!(
            windows_bundle_path_expression(r"\\server\share\o'hare.bundle"),
            Err(WindowsCommandError::QuotedRelativeBundlePath(
                r"\\server\share\o'hare.bundle".to_owned()
            ))
        );
    }

    #[test]
    fn bundle_probe_parser_accepts_banner_before_ok_sentinel() {
        let probe = parse_bundle_probe_stdout(
            "PowerShell profile banner\nOK size=42 mtime=2026-04-24T20:00:00Z path=C:/Users/d/shipyard.bundle\n",
        );

        assert!(probe.exists);
        assert_eq!(probe.size, 42);
        assert_eq!(
            probe.detail,
            "OK size=42 mtime=2026-04-24T20:00:00Z path=C:/Users/d/shipyard.bundle"
        );
    }

    #[test]
    fn bundle_probe_parser_reports_missing_sentinel() {
        let probe = parse_bundle_probe_stdout("banner\nMISSING path=C:/Users/d/shipyard.bundle\n");

        assert!(!probe.exists);
        assert_eq!(probe.size, 0);
        assert_eq!(probe.detail, "MISSING path=C:/Users/d/shipyard.bundle");
    }

    #[test]
    fn bundle_probe_parser_fails_closed_on_unexpected_output() {
        let probe = parse_bundle_probe_stdout("Preparing modules for first use.");

        assert!(!probe.exists);
        assert_eq!(probe.size, 0);
        assert!(probe.detail.contains("probe unexpected output"));
    }

    #[test]
    fn apply_bundle_script_preverifies_bundle_and_keeps_prelude() {
        let script = windows_apply_bundle_script("shipyard.bundle", "~/repo").expect("script");
        assert!(script.starts_with(WINDOWS_UTF8_PRELUDE));
        assert!(script.contains("$Bundle = (Join-Path $HOME 'shipyard.bundle')"));
        assert!(script.contains("Test-Path -LiteralPath $Bundle"));
        assert!(script.contains("bundle file not found"));
        assert!(script.contains("git bundle verify $Bundle"));
        assert!(script.contains("git fetch $Bundle"));
    }

    #[test]
    fn remote_command_prepends_prelude_to_single_command() {
        let command = build_remote_command(
            "abc123",
            "C:/repo",
            &WindowsValidation::Command("ctest --output-on-failure".to_owned()),
            None,
        )
        .expect("command");
        assert!(command.starts_with(WINDOWS_UTF8_PRELUDE));
        assert!(command.contains("cd 'C:/repo'"));
        assert!(command.contains("git checkout --force 'abc123'"));
        assert!(command.contains("ctest --output-on-failure"));
    }

    #[test]
    fn remote_command_prepends_prelude_to_staged_validation() {
        let mut stages = BTreeMap::new();
        stages.insert("build".to_owned(), "cmake --build build".to_owned());
        stages.insert("test".to_owned(), "ctest --output-on-failure".to_owned());
        let command = build_remote_command(
            "feedfacecafebeef",
            "C:/repo",
            &WindowsValidation::Stages(stages),
            None,
        )
        .expect("command");
        assert!(command.starts_with(WINDOWS_UTF8_PRELUDE));
        assert!(command.contains("Write-Output '__SHIPYARD_PHASE__:build'"));
        assert!(command.contains("cmake --build build"));
        assert!(command.contains("Write-Output '__SHIPYARD_PHASE__:test'"));
        assert!(command.contains("ctest --output-on-failure"));
        assert!(command.contains(".shipyard-stage-build-feedfacecafe"));
    }

    #[test]
    fn staged_validation_honors_resume_from() {
        let mut stages = BTreeMap::new();
        stages.insert("setup".to_owned(), "setup.ps1".to_owned());
        stages.insert("build".to_owned(), "build.ps1".to_owned());
        stages.insert("test".to_owned(), "test.ps1".to_owned());
        let command = build_remote_command(
            "feedfacecafebeef",
            "C:/repo",
            &WindowsValidation::Stages(stages),
            Some("build"),
        )
        .expect("command");
        assert!(!command.contains("setup.ps1"));
        assert!(command.contains("build.ps1"));
        assert!(command.contains("test.ps1"));
    }

    #[test]
    fn remote_command_includes_toolchain_exports() {
        let toolchain = super::VsToolchain {
            cmake_platform: "x64".to_owned(),
            cmake_generator_instance: "C:/VS".to_owned(),
        };
        let command = build_remote_command_with_toolchain(
            "abc123",
            "C:/repo",
            &WindowsValidation::Command("cmake --build build".to_owned()),
            Some(&toolchain),
            None,
        )
        .expect("command");

        assert!(command.contains("$env:SHIPYARD_CMAKE_PLATFORM = 'x64'"));
        assert!(command.contains("$env:SHIPYARD_CMAKE_GENERATOR_INSTANCE = 'C:/VS'"));
        assert!(command.contains("cmake --build build"));
    }

    #[test]
    fn executor_skips_bundle_when_remote_already_has_sha() {
        let operations = FakeWindowsOperations::passing();
        let executor = WindowsExecutor::with_operations(operations);

        let result = executor.validate(windows_request(WindowsValidation::Command(
            "ctest".to_owned(),
        )));

        assert_eq!(result.status, TargetStatus::Pass);
        let calls = executor.operations.calls.borrow();
        assert!(calls.create_basis.is_empty());
        assert_eq!(calls.upload_count, 0);
        assert_eq!(calls.apply_count, 0);
        assert_eq!(calls.stream_argv.len(), 1);
        let script = decode_encoded_ssh_argv(&calls.stream_argv[0]).expect("encoded script");
        assert!(script.contains("$MutexName = 'Global\\ShipyardValidate'"));
        assert!(script.contains("git checkout --force 'feedfacecafebeef'"));
        assert!(script.contains("ctest"));
    }

    #[test]
    fn executor_reports_missing_host_as_clean_error() {
        let executor = WindowsExecutor::with_operations(FakeWindowsOperations::passing());
        let mut request = windows_request(WindowsValidation::Command("ctest".to_owned()));
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
    fn executor_delivers_delta_bundle_and_logs_mode() {
        let mut operations = FakeWindowsOperations::passing();
        operations.remote_has_sha = false;
        operations.remote_head = Some("abc1234".to_owned());
        let executor = WindowsExecutor::with_operations(operations);
        let request = windows_request(WindowsValidation::Command("ctest".to_owned()));
        let log_path = request.log_path.clone();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Pass);
        let calls = executor.operations.calls.borrow();
        assert_eq!(calls.create_basis, vec![vec!["abc1234".to_owned()]]);
        assert_eq!(calls.upload_count, 1);
        assert_eq!(calls.probe_count, 1);
        assert_eq!(calls.apply_count, 1);
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("bundle_mode=delta"));
        assert!(log.contains("remote_head=abc1234"));
        assert!(log.contains("bundle post-upload probe: OK size=123"));
    }

    #[test]
    fn executor_falls_back_to_full_bundle_when_incremental_create_fails() {
        let mut operations = FakeWindowsOperations::passing();
        operations.remote_has_sha = false;
        operations.remote_head = Some("abc1234".to_owned());
        operations.create_results = RefCell::new(VecDeque::from([
            BundleResult::failure("incremental failed"),
            BundleResult::success("created", "shipyard.bundle"),
        ]));
        let executor = WindowsExecutor::with_operations(operations);

        let result = executor.validate(windows_request(WindowsValidation::Command(
            "ctest".to_owned(),
        )));

        assert_eq!(result.status, TargetStatus::Pass);
        assert_eq!(
            executor.operations.calls.borrow().create_basis,
            vec![vec!["abc1234".to_owned()], Vec::<String>::new()]
        );
    }

    #[test]
    fn executor_stops_before_validation_when_apply_fails() {
        let mut operations = FakeWindowsOperations::passing();
        operations.remote_has_sha = false;
        operations.apply_result = BundleResult::failure("apply broke");
        let executor = WindowsExecutor::with_operations(operations);

        let result = executor.validate(windows_request(WindowsValidation::Command(
            "ctest".to_owned(),
        )));

        assert_eq!(result.status, TargetStatus::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("Bundle apply failed: apply broke")
        );
        assert!(executor.operations.calls.borrow().stream_argv.is_empty());
    }

    #[test]
    fn executor_logs_and_classifies_unreachable_bundle_upload_failure() {
        let mut operations = FakeWindowsOperations::passing();
        operations.remote_has_sha = false;
        operations.upload_result = BundleResult::upload_failure(
            "Upload failed after 1 attempt(s): ssh: connect to host 100.92.174.43 port 22: Operation timed out",
            "ssh_unreachable",
            vec![
                "attempt 1/3 failed after 30.0s: ssh: connect to host 100.92.174.43 port 22: Operation timed out"
                    .to_owned(),
            ],
        );
        let executor = WindowsExecutor::with_operations(operations);
        let request = windows_request(WindowsValidation::Command("ctest".to_owned()));
        let log_path = request.log_path.clone();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Error);
        assert!(
            result
                .error_message
                .as_deref()
                .expect("error")
                .contains("Bundle upload failed [ssh-unreachable]")
        );
        let calls = executor.operations.calls.borrow();
        assert_eq!(calls.upload_count, 1);
        assert_eq!(calls.probe_count, 0);
        assert_eq!(calls.apply_count, 0);
        assert!(calls.stream_argv.is_empty());
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("# shipyard ssh-windows lane log"));
        assert!(log.contains("remote_bundle: shipyard.bundle"));
        assert!(log.contains("bundle-upload failure (class=ssh_unreachable)"));
        assert!(log.contains("attempt 1/3 failed"));
        assert!(log.contains("summary: Upload failed after 1 attempt(s)"));
    }

    #[test]
    fn executor_logs_and_classifies_reachable_bundle_upload_failure() {
        let mut operations = FakeWindowsOperations::passing();
        operations.remote_has_sha = false;
        operations.upload_result = BundleResult::upload_failure(
            "Upload failed after 3 attempt(s): scp: stream ended unexpectedly",
            "upload_failed",
            vec![
                "attempt 1/3 failed after 10.0s: scp: stream ended unexpectedly".to_owned(),
                "attempt 2/3 failed after 10.0s: scp: stream ended unexpectedly".to_owned(),
                "attempt 3/3 failed after 10.0s: scp: stream ended unexpectedly".to_owned(),
            ],
        );
        let executor = WindowsExecutor::with_operations(operations);
        let request = windows_request(WindowsValidation::Command("ctest".to_owned()));
        let log_path = request.log_path.clone();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Error);
        assert!(
            result
                .error_message
                .as_deref()
                .expect("error")
                .contains("Bundle upload failed [upload failed after reachable]")
        );
        let calls = executor.operations.calls.borrow();
        assert_eq!(calls.upload_count, 1);
        assert_eq!(calls.probe_count, 0);
        assert_eq!(calls.apply_count, 0);
        assert!(calls.stream_argv.is_empty());
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("bundle-upload failure (class=upload_failed)"));
        assert!(log.contains("attempt 3/3 failed"));
        assert!(log.contains("summary: Upload failed after 3 attempt(s)"));
    }

    #[test]
    fn executor_stops_before_apply_when_uploaded_bundle_is_missing() {
        let mut operations = FakeWindowsOperations::passing();
        operations.remote_has_sha = false;
        operations.probe_result = BundleProbe::missing("MISSING path=C:/Users/d/shipyard.bundle");
        let executor = WindowsExecutor::with_operations(operations);

        let result = executor.validate(windows_request(WindowsValidation::Command(
            "ctest".to_owned(),
        )));

        assert_eq!(result.status, TargetStatus::Error);
        assert!(
            result
                .error_message
                .as_deref()
                .expect("error")
                .contains("remote file is missing: MISSING path=C:/Users/d/shipyard.bundle")
        );
        let calls = executor.operations.calls.borrow();
        assert_eq!(calls.upload_count, 1);
        assert_eq!(calls.probe_count, 1);
        assert_eq!(calls.apply_count, 0);
        assert!(calls.stream_argv.is_empty());
    }

    #[test]
    fn executor_stops_before_apply_when_uploaded_bundle_is_zero_bytes() {
        let mut operations = FakeWindowsOperations::passing();
        operations.remote_has_sha = false;
        operations.probe_result = BundleProbe::present(
            0,
            "OK size=0 mtime=2026-04-24T20:00:00Z path=shipyard.bundle",
        );
        let executor = WindowsExecutor::with_operations(operations);

        let result = executor.validate(windows_request(WindowsValidation::Command(
            "ctest".to_owned(),
        )));

        assert_eq!(result.status, TargetStatus::Error);
        assert!(
            result
                .error_message
                .as_deref()
                .expect("error")
                .contains("remote file is 0 bytes")
        );
        let calls = executor.operations.calls.borrow();
        assert_eq!(calls.probe_count, 1);
        assert_eq!(calls.apply_count, 0);
    }

    #[test]
    fn executor_waits_for_uploaded_bundle_to_reach_expected_size() {
        let mut operations = FakeWindowsOperations::passing();
        operations.remote_has_sha = false;
        operations.created_bundle_bytes = Some(5);
        operations.probe_results = RefCell::new(VecDeque::from([
            BundleProbe::present(
                2,
                "OK size=2 mtime=2026-04-24T20:00:00Z path=shipyard.bundle",
            ),
            BundleProbe::present(
                5,
                "OK size=5 mtime=2026-04-24T20:00:01Z path=shipyard.bundle",
            ),
        ]));
        let executor = WindowsExecutor::with_operations(operations);
        let request = windows_request(WindowsValidation::Command("ctest".to_owned()));
        let log_path = request.log_path.clone();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Pass);
        let calls = executor.operations.calls.borrow();
        assert_eq!(calls.probe_count, 2);
        assert_eq!(calls.apply_count, 1);
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("waiting for complete size"));
    }

    #[test]
    fn executor_enforces_validation_contract_on_zero_exit() {
        let executor = WindowsExecutor::with_operations(FakeWindowsOperations::passing());
        let mut request = windows_request(WindowsValidation::Command("ctest".to_owned()));
        request.contract = Some(ContractConfig::new(vec!["SMOKE_DONE".to_owned()]));

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Fail);
        assert_eq!(result.failure_class.as_deref(), Some("CONTRACT"));
        assert_eq!(
            result.contract_markers_missing,
            vec!["SMOKE_DONE".to_owned()]
        );
    }

    #[test]
    fn executor_decodes_clixml_transport_error() {
        let operations = FakeWindowsOperations {
            stream_results: RefCell::new(VecDeque::from([Ok(stream_result(
                255,
                "#< CLIXML\n<Objs><S S=\"Error\">inner PS error</S></Objs>",
                Vec::new(),
            ))])),
            ..FakeWindowsOperations::passing()
        };
        let executor = WindowsExecutor::with_operations(operations);

        let result = executor.validate(windows_request(WindowsValidation::Command(
            "ctest".to_owned(),
        )));

        assert_eq!(result.status, TargetStatus::Error);
        assert_eq!(result.error_message.as_deref(), Some("inner PS error"));
    }

    #[test]
    fn executor_honors_resume_from_when_previous_marker_exists() {
        let mut operations = FakeWindowsOperations::passing();
        operations.marker_exists = true;
        operations.toolchain = Some(super::VsToolchain {
            cmake_platform: "ARM64".to_owned(),
            cmake_generator_instance: "C:/VS".to_owned(),
        });
        let executor = WindowsExecutor::with_operations(operations);
        let mut request = windows_request(WindowsValidation::Stages(stage_map(&[
            ("setup", "setup.ps1"),
            ("build", "build.ps1"),
            ("test", "test.ps1"),
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
        let script = decode_encoded_ssh_argv(calls.stream_argv.last().expect("stream argv"))
            .expect("script");
        assert!(!script.contains("setup.ps1"));
        assert!(script.contains("build.ps1"));
        assert!(script.contains("$env:SHIPYARD_CMAKE_PLATFORM = 'ARM64'"));
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("resume-from: honoring"));
    }

    #[test]
    fn executor_runs_all_stages_when_resume_marker_is_missing() {
        let executor = WindowsExecutor::with_operations(FakeWindowsOperations::passing());
        let mut request = windows_request(WindowsValidation::Stages(stage_map(&[
            ("setup", "setup.ps1"),
            ("build", "build.ps1"),
        ])));
        request.resume_from = Some("build".to_owned());
        let log_path = request.log_path.clone();

        let result = executor.validate(request);

        assert_eq!(result.status, TargetStatus::Pass);
        let calls = executor.operations.calls.borrow();
        let script = decode_encoded_ssh_argv(calls.stream_argv.last().expect("stream argv"))
            .expect("script");
        assert!(script.contains("setup.ps1"));
        assert!(script.contains("build.ps1"));
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("running all stages from the beginning"));
    }
}
