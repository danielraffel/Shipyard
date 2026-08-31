//! Local executor planning helpers.
//!
//! These helpers keep local validation mode selection independent from
//! process execution so staged behavior, resume filtering, and
//! prepared-state opt-in can be tested without spawning commands.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::classify::{FailureClass, classify_failure};
use crate::executor::contract::{ContractConfig, evaluate_contract, required_markers};
use crate::executor::streaming::{
    ProgressEvent, StreamingCommand, StreamingError, run_streaming_command,
};
use crate::job::{TargetResult, TargetStatus};
use crate::prepared_state::{
    PreparedStateRecord, PreparedStateStore, filter_stages_by_prepared_state, hash_stage_commands,
};
use crate::tree_drift;

const STAGE_ORDER: [&str; 4] = ["setup", "configure", "build", "test"];

/// A configured validation stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StageCommand {
    /// Stage name.
    pub stage: String,
    /// Shell command to execute.
    pub command: String,
}

/// Planned local validation shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalValidationPlan {
    /// Run one shell command.
    SingleCommand(String),
    /// Run ordered validation stages.
    Stages(Vec<StageCommand>),
    /// No command or stage is configured.
    Missing,
}

/// Build the local validation plan.
///
/// A single command takes precedence over staged config, matching the
/// Python executor. Staged plans honor `resume_from` by skipping earlier
/// configured stages.
#[must_use]
pub fn plan_validation(
    command: Option<&str>,
    stages: &BTreeMap<String, String>,
    resume_from: Option<&str>,
) -> LocalValidationPlan {
    if let Some(command) = command
        && !command.is_empty()
    {
        return LocalValidationPlan::SingleCommand(command.to_owned());
    }

    let stages = configured_stages(stages, resume_from);
    if stages.is_empty() {
        LocalValidationPlan::Missing
    } else {
        LocalValidationPlan::Stages(stages)
    }
}

/// Extract configured stages in Shipyard stage order.
#[must_use]
pub fn configured_stages(
    stages: &BTreeMap<String, String>,
    resume_from: Option<&str>,
) -> Vec<StageCommand> {
    let mut selected = Vec::new();
    let mut skipping = resume_from.is_some();

    for stage in STAGE_ORDER {
        let Some(command) = stages.get(stage) else {
            continue;
        };
        if command.is_empty() {
            continue;
        }
        if skipping {
            if Some(stage) == resume_from {
                skipping = false;
            } else {
                continue;
            }
        }
        selected.push(StageCommand {
            stage: stage.to_owned(),
            command: command.clone(),
        });
    }

    selected
}

/// Read `[validation.prepared_state] enabled = true`.
#[must_use]
pub fn prepared_state_enabled(section: Option<&BTreeMap<String, bool>>) -> bool {
    section
        .and_then(|section| section.get("enabled"))
        .copied()
        .unwrap_or(false)
}

/// Read the tail of a log file for failure classification.
///
/// Any I/O error returns an empty string so classification can fall back
/// safely rather than masking the original validation failure.
#[must_use]
pub fn read_log_tail(path: &Path, max_bytes: u64) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let Ok(size) = file.metadata().map(|metadata| metadata.len()) else {
        return String::new();
    };
    let tail_offset = i64::try_from(max_bytes).unwrap_or(i64::MAX);
    if size > max_bytes && file.seek(SeekFrom::End(-tail_offset)).is_err() {
        return String::new();
    }

    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Local target execution settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalTargetConfig {
    /// Logical target name.
    pub name: String,
    /// Platform label.
    pub platform: String,
    /// Optional working directory.
    pub cwd: Option<PathBuf>,
    /// Wall-clock timeout in seconds.
    pub timeout_secs: u64,
}

impl Default for LocalTargetConfig {
    fn default() -> Self {
        Self {
            name: "local".to_owned(),
            platform: "unknown".to_owned(),
            cwd: None,
            timeout_secs: 1_800,
        }
    }
}

impl LocalTargetConfig {
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

/// Local validation settings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalValidationConfig {
    /// Optional single command. Takes precedence over stages.
    pub command: Option<String>,
    /// Stage commands keyed by stage name.
    pub stages: BTreeMap<String, String>,
    /// Optional validation contract.
    pub contract: Option<ContractConfig>,
    /// Whether prepared-state stage skipping is enabled.
    pub prepared_state_enabled: bool,
    /// Suppress staged working-tree drift detection.
    pub allow_tree_drift: bool,
    /// Names requested from the trusted machine-global project environment.
    pub machine_environment: Vec<String>,
    /// Resolved trusted environment values applied to every validation stage.
    pub environment: BTreeMap<String, String>,
    /// Exact isolated integration checkout removed after terminal validation.
    pub(crate) integration_cleanup:
        Option<Box<crate::changed_surface::integration_checkout::IntegrationCheckout>>,
}

/// Request for one local validation run.
pub struct LocalValidationRequest<'a> {
    /// Commit SHA under validation.
    pub sha: String,
    /// Branch under validation.
    pub branch: String,
    /// Target execution settings.
    pub target: LocalTargetConfig,
    /// Validation command/stage settings.
    pub validation: LocalValidationConfig,
    /// Log file path.
    pub log_path: PathBuf,
    /// Optional stage to resume from.
    pub resume_from: Option<String>,
    /// Validation mode label.
    pub mode: String,
    /// Optional progress callback.
    pub progress_callback:
        Option<&'a mut dyn FnMut(ProgressEvent) -> crate::executor::streaming::ProgressAction>,
}

impl LocalValidationRequest<'_> {
    /// Create a request with Python-compatible defaults.
    #[must_use]
    pub fn new(log_path: PathBuf, validation: LocalValidationConfig) -> Self {
        Self {
            sha: String::new(),
            branch: String::new(),
            target: LocalTargetConfig::default(),
            validation,
            log_path,
            resume_from: None,
            mode: "default".to_owned(),
            progress_callback: None,
        }
    }
}

/// Execute validation commands locally.
#[derive(Clone, Debug)]
pub struct LocalExecutor {
    prepared_state_store: Option<PreparedStateStore>,
    rotated_segments: usize,
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new(None)
    }
}

impl LocalExecutor {
    /// Construct a local executor with optional prepared-state storage.
    #[must_use]
    pub fn new(prepared_state_store: Option<PreparedStateStore>) -> Self {
        Self {
            prepared_state_store,
            rotated_segments: crate::log_retention::LogRetentionPolicy::default().rotated_segments,
        }
    }

    /// Apply the effective target-log reopen retention policy.
    #[must_use]
    pub fn with_rotated_segments(mut self, rotated_segments: usize) -> Self {
        self.rotated_segments = rotated_segments;
        self
    }

    /// Run a local validation request and return a target result.
    #[must_use]
    pub fn validate(&self, mut request: LocalValidationRequest<'_>) -> TargetResult {
        let started_at = Utc::now();
        let start_time = Instant::now();
        let progress_callback = request.progress_callback.take();
        let full_execution =
            request.resume_from.is_none() && !request.validation.prepared_state_enabled;
        let plan = plan_validation(
            request.validation.command.as_deref(),
            &request.validation.stages,
            request.resume_from.as_deref(),
        );
        let mut environment = request.validation.environment.clone();
        let _validation_temp_dir = match isolate_protected_inherited_tmpdir(&mut environment) {
            Ok(temp_dir) => temp_dir,
            Err(error) => {
                let context = LocalRunContext {
                    target: &request.target,
                    environment: &environment,
                    log_path: &request.log_path,
                    started_at,
                    start_time,
                };
                return io_error_result(&context, &error.to_string());
            }
        };
        let context = LocalRunContext {
            target: &request.target,
            environment: &environment,
            log_path: &request.log_path,
            started_at,
            start_time,
        };
        let source_cwd = request
            .target
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok());
        let source = source_cwd.as_deref().and_then(source_provenance);

        let mut result = match plan {
            LocalValidationPlan::SingleCommand(command) => self.run_single(
                &command,
                &context,
                request.validation.contract.as_ref(),
                progress_callback,
            ),
            LocalValidationPlan::Stages(stages) => self.run_stages(
                &stages,
                &request.validation,
                RunIdentity {
                    sha: &request.sha,
                    mode: &request.mode,
                },
                &context,
                progress_callback,
            ),
            LocalValidationPlan::Missing => missing_command_result(
                &request.target,
                started_at,
                Some(start_time.elapsed().as_secs_f64()),
            ),
        };
        if let Some((head, tree, clean)) = source {
            result.source_head_sha = Some(head);
            result.source_tree_sha = Some(tree);
            result.source_checkout_clean = Some(clean);
        }
        result.full_execution = Some(full_execution);
        if let Some(cleanup) = request.validation.integration_cleanup.as_ref()
            && let Err(error) = crate::changed_surface::integration_checkout::cleanup(cleanup)
        {
            result.status = TargetStatus::Error;
            result.error_message = Some(format!(
                "integration checkout cleanup uncertain; preserved for reconciliation: {error}"
            ));
            result.failure_class = Some(FailureClass::Unknown.as_str().to_owned());
        }
        result
    }

    fn run_single(
        &self,
        command: &str,
        context: &LocalRunContext<'_>,
        contract: Option<&ContractConfig>,
        mut progress_callback: Option<
            &mut dyn FnMut(ProgressEvent) -> crate::executor::streaming::ProgressAction,
        >,
    ) -> TargetResult {
        let mut request = StreamingCommand::shell(command);
        request.cwd.clone_from(&context.target.cwd);
        request.environment.clone_from(context.environment);
        request.log_path = Some(context.log_path.to_path_buf());
        request.rotated_segments = self.rotated_segments;
        request.timeout = Some(context.target.timeout());
        request.required_contract_markers = required_markers(contract);
        request.progress_callback = progress_callback.take();

        match run_streaming_command(request) {
            Ok(result) => single_result(context, contract, result),
            Err(error) => streaming_error_result(context, error),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn run_stages(
        &self,
        stages: &[StageCommand],
        validation: &LocalValidationConfig,
        identity: RunIdentity<'_>,
        context: &LocalRunContext<'_>,
        mut progress_callback: Option<
            &mut dyn FnMut(ProgressEvent) -> crate::executor::streaming::ProgressAction,
        >,
    ) -> TargetResult {
        if let Some(parent) = context.log_path.parent()
            && let Err(error) = crate::writer_domain_lease::ensure_protected_dir_all(parent)
        {
            return io_error_result(context, &error.to_string());
        }
        if let Err(error) =
            crate::log_retention::rotate_before_open(context.log_path, self.rotated_segments)
        {
            return io_error_result(context, &error.to_string());
        }
        let writer_domain =
            match crate::writer_domain_lease::acquire_for_protected_path(context.log_path) {
                Ok(lease) => lease,
                Err(error) => return io_error_result(context, &error.to_string()),
            };
        if let Err(error) = fs::write(context.log_path, "") {
            return io_error_result(context, &error.to_string());
        }
        drop(writer_domain);

        let stage_pairs = stages
            .iter()
            .map(|stage| (stage.stage.clone(), stage.command.clone()))
            .collect::<Vec<_>>();
        let mut prepared_state_inputs = stage_pairs.clone();
        prepared_state_inputs.extend(validation.environment.iter().map(|(name, value)| {
            // The NUL-prefixed namespace cannot collide with configured stage
            // names and keeps resolved values out of the persisted record.
            (format!("\0environment:{name}"), value.clone())
        }));
        let config_hash = hash_stage_commands(&prepared_state_inputs);
        let store = validation
            .prepared_state_enabled
            .then_some(self.prepared_state_store.as_ref())
            .flatten()
            .filter(|_| !identity.sha.is_empty());
        let (stages_to_run, skipped, mut record) =
            Self::prepare_stage_run(store, &stage_pairs, &config_hash, context.target, identity);

        if !skipped.is_empty() {
            let text = format!(
                "=== prepared-state-reuse: skipped {} stage(s) ({}) for sha={} target={} mode={} ===\n",
                skipped.len(),
                skipped.join(", "),
                identity.sha,
                context.target.name,
                identity.mode
            );
            if let Err(error) = append_log(context.log_path, &text) {
                return io_error_result(context, &error.to_string());
            }
        }

        let mut seen_markers = Vec::new();
        let mut last_output_at = None;
        let mut final_phase = None;
        let mut failed_stage = None;
        let markers_to_watch = required_markers(validation.contract.as_ref());
        let drift_cwd = (!validation.allow_tree_drift)
            .then(|| {
                context
                    .target
                    .cwd
                    .clone()
                    .or_else(|| std::env::current_dir().ok())
            })
            .flatten();
        let initial_signature = drift_cwd.as_deref().and_then(tree_drift::compute_signature);
        let initial_dirty_paths = drift_cwd
            .as_deref()
            .filter(|_| initial_signature.is_some())
            .map(tree_drift::list_dirty_paths)
            .unwrap_or_default();

        for (index, (stage_name, command)) in stages_to_run.iter().enumerate() {
            if index > 0
                && let (Some(cwd), Some(initial)) =
                    (drift_cwd.as_deref(), initial_signature.as_ref())
                && let Some(current) = tree_drift::compute_signature(cwd)
                && &current != initial
            {
                let current_dirty_paths = tree_drift::list_dirty_paths(cwd);
                let error = tree_drift::format_drift_error(
                    stage_name,
                    &initial_dirty_paths,
                    &current_dirty_paths,
                );
                if let Err(error) = append_log(
                    context.log_path,
                    &format!("\n=== TREE_DRIFT at {stage_name} ===\n{error}\n"),
                ) {
                    return io_error_result(context, &error.to_string());
                }
                return tree_drift_result(context, stage_name, error);
            }
            if let Err(error) = append_log(context.log_path, &format!("\n=== {stage_name} ===\n")) {
                return io_error_result(context, &error.to_string());
            }
            if let Some(callback) = progress_callback.as_mut()
                && let crate::executor::streaming::ProgressAction::Terminate(reason) =
                    callback(ProgressEvent::phase(stage_name))
            {
                if let Err(error) = append_log(
                    context.log_path,
                    &format!("\n=== CANCELLED before {stage_name}: {reason} ===\n"),
                ) {
                    return io_error_result(context, &error.to_string());
                }
                let mut result = error_result(
                    context.target,
                    context.log_path,
                    context.started_at,
                    Some(context.start_time.elapsed().as_secs_f64()),
                );
                result.status = TargetStatus::Cancelled;
                result.phase = Some(stage_name.clone());
                result.error_message = Some(reason);
                result.failure_class = None;
                return result;
            }

            let stage_run = {
                let mut request = StreamingCommand::shell(command);
                request.cwd.clone_from(&context.target.cwd);
                request.environment.clone_from(context.environment);
                request.log_path = Some(context.log_path.to_path_buf());
                request.append = true;
                request.timeout = Some(context.target.timeout());
                request.phase = Some(stage_name.clone());
                request
                    .required_contract_markers
                    .clone_from(&markers_to_watch);
                if let Some(callback) = progress_callback.as_mut() {
                    request.progress_callback = Some(&mut **callback);
                }
                run_streaming_command(request)
            };
            let result = match stage_run {
                Ok(result) => result,
                Err(error) => {
                    return streaming_error_result(context, error);
                }
            };

            final_phase = Some(stage_name.clone());
            last_output_at = result.last_output_at;
            for marker in result.contract_markers_seen {
                if !seen_markers.contains(&marker) {
                    seen_markers.push(marker);
                }
            }

            if let Some(store) = store {
                let outcome = if result.returncode == 0 {
                    "pass"
                } else {
                    "fail"
                };
                if let Err(error) = save_stage_outcome(
                    store,
                    &mut record,
                    identity,
                    context.target,
                    &config_hash,
                    stage_name,
                    outcome,
                ) {
                    return io_error_result(context, &error.to_string());
                }
            }

            if result.returncode != 0 {
                failed_stage = Some(stage_name.clone());
                break;
            }
        }

        let summary = StageRunSummary {
            seen_markers,
            failed_stage,
            final_phase,
            last_output_at,
        };
        stages_result(context, validation.contract.as_ref(), &summary)
    }

    fn prepare_stage_run(
        store: Option<&PreparedStateStore>,
        stage_pairs: &[(String, String)],
        config_hash: &str,
        target: &LocalTargetConfig,
        identity: RunIdentity<'_>,
    ) -> (
        Vec<(String, String)>,
        Vec<String>,
        Option<PreparedStateRecord>,
    ) {
        let Some(store) = store else {
            return (stage_pairs.to_vec(), Vec::new(), None);
        };

        let mut record = store.get(identity.sha, &target.name, identity.mode);
        if record
            .as_ref()
            .is_some_and(|record| record.config_hash != config_hash)
        {
            let _ = store.delete(identity.sha, &target.name, identity.mode);
            record = None;
        }
        let (stages_to_run, skipped) =
            filter_stages_by_prepared_state(stage_pairs, record.as_ref(), config_hash);
        (stages_to_run, skipped, record)
    }
}

#[cfg(unix)]
fn isolate_protected_inherited_tmpdir(
    environment: &mut BTreeMap<String, String>,
) -> std::io::Result<Option<tempfile::TempDir>> {
    if environment.contains_key("TMPDIR") {
        return Ok(None);
    }
    let Some(inherited_tmpdir) = std::env::var_os("TMPDIR") else {
        return Ok(None);
    };
    isolate_protected_tmpdir(environment, Path::new(&inherited_tmpdir))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // Keep one fallible cross-platform isolation contract.
fn isolate_protected_inherited_tmpdir(
    _environment: &mut BTreeMap<String, String>,
) -> std::io::Result<Option<tempfile::TempDir>> {
    Ok(None)
}

#[cfg(unix)]
fn isolate_protected_tmpdir(
    environment: &mut BTreeMap<String, String>,
    inherited_tmpdir: &Path,
) -> std::io::Result<Option<tempfile::TempDir>> {
    if !crate::writer_domain_lease::is_current_protected_path(inherited_tmpdir)? {
        return Ok(None);
    }
    #[cfg(target_os = "macos")]
    let base = Path::new("/private/tmp");
    #[cfg(not(target_os = "macos"))]
    let base = Path::new("/tmp");
    let metadata = std::fs::symlink_metadata(base)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "validation temporary base {} is not a real directory",
            base.display()
        )));
    }
    let temp_dir = tempfile::Builder::new()
        .prefix("shipyard-validation-")
        .tempdir_in(base)?;
    std::fs::set_permissions(temp_dir.path(), std::fs::Permissions::from_mode(0o700))?;
    let value = temp_dir.path().to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "validation temporary path is not valid UTF-8",
        )
    })?;
    environment.insert("TMPDIR".to_owned(), value.to_owned());
    Ok(Some(temp_dir))
}

fn source_provenance(cwd: &Path) -> Option<(String, String, bool)> {
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let head = git(&["rev-parse", "HEAD"])?;
    let tree = git(&["rev-parse", "HEAD^{tree}"])?;
    let status = git(&["status", "--porcelain", "--untracked-files=normal"])?;
    Some((head, tree, status.is_empty()))
}

#[derive(Clone, Copy)]
struct RunIdentity<'a> {
    sha: &'a str,
    mode: &'a str,
}

struct LocalRunContext<'a> {
    target: &'a LocalTargetConfig,
    environment: &'a BTreeMap<String, String>,
    log_path: &'a Path,
    started_at: DateTime<Utc>,
    start_time: Instant,
}

struct StageRunSummary {
    seen_markers: Vec<String>,
    failed_stage: Option<String>,
    final_phase: Option<String>,
    last_output_at: Option<DateTime<Utc>>,
}

fn single_result(
    context: &LocalRunContext<'_>,
    contract: Option<&ContractConfig>,
    result: crate::executor::streaming::StreamingCommandResult,
) -> TargetResult {
    let mut status = if result.returncode == 0 {
        TargetStatus::Pass
    } else {
        TargetStatus::Fail
    };
    let evaluation = evaluate_contract(contract, &result.contract_markers_seen);
    if evaluation.should_force_fail() && status == TargetStatus::Pass {
        status = TargetStatus::Fail;
    }

    let mut target_result = TargetResult::new(
        context.target.name.clone(),
        context.target.platform.clone(),
        status,
        "local",
    );
    target_result.duration_secs = Some(result.duration_secs);
    target_result.started_at = Some(context.started_at);
    target_result.completed_at = Some(result.completed_at);
    target_result.log_path = Some(context.log_path.display().to_string());
    target_result.phase = result.phase;
    target_result.last_output_at = result.last_output_at;
    target_result.last_heartbeat_at = result.last_heartbeat_at;
    target_result.contract_markers_seen = evaluation.seen;
    target_result.contract_markers_missing = evaluation.missing;
    target_result.contract_violation = evaluation.message;
    if status != TargetStatus::Pass {
        target_result.failure_class = Some(
            classify_failure(
                "",
                &read_log_tail(context.log_path, 8192),
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

fn stages_result(
    context: &LocalRunContext<'_>,
    contract: Option<&ContractConfig>,
    summary: &StageRunSummary,
) -> TargetResult {
    let evaluation = evaluate_contract(contract, &summary.seen_markers);
    let mut status = if summary.failed_stage.is_some() {
        TargetStatus::Fail
    } else {
        TargetStatus::Pass
    };
    if evaluation.should_force_fail() {
        status = TargetStatus::Fail;
    }

    let mut result = TargetResult::new(
        context.target.name.clone(),
        context.target.platform.clone(),
        status,
        "local",
    );
    result.duration_secs = Some(context.start_time.elapsed().as_secs_f64());
    result.started_at = Some(context.started_at);
    result.completed_at = Some(Utc::now());
    result.log_path = Some(context.log_path.display().to_string());
    result.phase = summary
        .failed_stage
        .clone()
        .or_else(|| summary.final_phase.clone());
    result.last_output_at = summary.last_output_at;
    result.error_message = summary
        .failed_stage
        .clone()
        .map(|stage| format!("Stage '{stage}' failed"))
        .or_else(|| {
            (status == TargetStatus::Fail)
                .then(|| evaluation.message.clone())
                .flatten()
        });
    result.contract_markers_seen = evaluation.seen;
    result.contract_markers_missing = evaluation.missing;
    result.contract_violation = evaluation.message;
    if status != TargetStatus::Pass {
        result.failure_class = Some(
            classify_failure(
                "",
                &read_log_tail(context.log_path, 8192),
                i32::from(status == TargetStatus::Fail),
                false,
                result.contract_violation.is_some() && contract.is_some_and(|c| c.enforce),
            )
            .as_str()
            .to_owned(),
        );
    }
    result
}

fn streaming_error_result(context: &LocalRunContext<'_>, error: StreamingError) -> TargetResult {
    match error {
        StreamingError::Timeout { .. } => {
            let mut result = error_result(
                context.target,
                context.log_path,
                context.started_at,
                Some(context.start_time.elapsed().as_secs_f64()),
            );
            result.error_message = Some("Validation timed out".to_owned());
            result.failure_class = Some(FailureClass::Timeout.as_str().to_owned());
            result
        }
        StreamingError::Io(error) => io_error_result(context, &error.to_string()),
        StreamingError::MissingProgram => {
            io_error_result(context, "streaming command has no program")
        }
    }
}

fn io_error_result(context: &LocalRunContext<'_>, message: &str) -> TargetResult {
    let mut result = error_result(
        context.target,
        context.log_path,
        context.started_at,
        Some(context.start_time.elapsed().as_secs_f64()),
    );
    result.error_message = Some(message.to_owned());
    result.failure_class = Some(
        classify_failure("", message, -1, false, false)
            .as_str()
            .to_owned(),
    );
    result
}

fn missing_command_result(
    target: &LocalTargetConfig,
    started_at: DateTime<Utc>,
    duration_secs: Option<f64>,
) -> TargetResult {
    let mut result = TargetResult::new(
        target.name.clone(),
        target.platform.clone(),
        TargetStatus::Error,
        "local",
    );
    result.started_at = Some(started_at);
    result.completed_at = Some(Utc::now());
    result.duration_secs = duration_secs;
    result.error_message = Some("No validation command configured".to_owned());
    result.failure_class = Some(FailureClass::Unknown.as_str().to_owned());
    result
}

fn tree_drift_result(
    context: &LocalRunContext<'_>,
    stage_name: &str,
    message: String,
) -> TargetResult {
    let mut result = error_result(
        context.target,
        context.log_path,
        context.started_at,
        Some(context.start_time.elapsed().as_secs_f64()),
    );
    result.error_message = Some(message);
    result.phase = Some(stage_name.to_owned());
    result.failure_class = Some(FailureClass::TreeDrift.as_str().to_owned());
    result
}

fn error_result(
    target: &LocalTargetConfig,
    log_path: &Path,
    started_at: DateTime<Utc>,
    duration_secs: Option<f64>,
) -> TargetResult {
    let mut result = TargetResult::new(
        target.name.clone(),
        target.platform.clone(),
        TargetStatus::Error,
        "local",
    );
    result.started_at = Some(started_at);
    result.completed_at = Some(Utc::now());
    result.duration_secs = duration_secs;
    result.log_path = Some(log_path.display().to_string());
    result
}

fn append_log(path: &Path, text: &str) -> std::io::Result<()> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
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

fn save_stage_outcome(
    store: &PreparedStateStore,
    record: &mut Option<PreparedStateRecord>,
    identity: RunIdentity<'_>,
    target: &LocalTargetConfig,
    config_hash: &str,
    stage_name: &str,
    outcome: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if record.is_none() {
        *record = Some(PreparedStateRecord::new(
            identity.sha,
            &target.name,
            identity.mode,
            config_hash,
        ));
    }
    let record = record.as_mut().expect("record initialized");
    record.mark(stage_name, outcome);
    store.save(record)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::Command;

    #[cfg(unix)]
    use super::isolate_protected_tmpdir;
    use super::{
        ContractConfig, LocalExecutor, LocalTargetConfig, LocalValidationConfig,
        LocalValidationPlan, LocalValidationRequest, StageCommand, TargetStatus, configured_stages,
        plan_validation, prepared_state_enabled, read_log_tail, source_provenance,
    };
    #[cfg(unix)]
    use crate::identity::RuntimeMode;
    #[cfg(unix)]
    use crate::paths::RuntimePaths;
    use crate::prepared_state::PreparedStateStore;

    fn stage_map(values: &[(&str, &str)]) -> BTreeMap<String, String> {
        values
            .iter()
            .map(|(stage, command)| ((*stage).to_owned(), (*command).to_owned()))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn daemon_owned_validation_moves_test_fixtures_out_of_the_protected_tmpdir() {
        let inherited = RuntimePaths::current(RuntimeMode::Shipyard)
            .state_dir
            .join("daemon/tmp");
        let mut environment = BTreeMap::new();
        let temp_dir = isolate_protected_tmpdir(&mut environment, &inherited)
            .expect("isolation")
            .expect("protected inherited root");
        let isolated = Path::new(environment.get("TMPDIR").expect("isolated TMPDIR"));

        assert_eq!(isolated, temp_dir.path());
        assert!(isolated.is_dir());
        assert!(
            !std::fs::symlink_metadata(isolated)
                .expect("metadata")
                .file_type()
                .is_symlink()
        );
        assert!(
            !crate::writer_domain_lease::is_current_protected_path(isolated)
                .expect("protected-path classification")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(isolated)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        for test_name in [
            "app::ship_cmd::prepush_changed_surface::tests::push_hook_receipt_and_postpush_equivalence_create_one_verified_snapshot",
            "app::update_cmd::tests::installer_receives_exact_binary_directory_and_curl_identity",
            "writer_domain_lease::tests::unrelated_test_path_never_opens_production_writer_domain",
        ] {
            let status = Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", test_name])
                .env("TMPDIR", isolated)
                .status()
                .expect("focused child test");
            assert!(
                status.success(),
                "{test_name} must pass in the isolated root"
            );
        }
    }

    #[test]
    fn source_provenance_binds_clean_head_and_tree_before_execution() {
        let repo = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("git");
            assert!(status.success());
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(repo.path().join("source.txt"), "clean\n").expect("source");
        git(&["add", "source.txt"]);
        git(&["commit", "-q", "-m", "initial"]);

        let (head, tree, clean) = source_provenance(repo.path()).expect("provenance");
        assert_eq!(head.len(), 40);
        assert_eq!(tree.len(), 40);
        assert!(clean);

        std::fs::write(repo.path().join("source.txt"), "dirty\n").expect("edit");
        let (_, same_tree, clean) = source_provenance(repo.path()).expect("provenance");
        assert_eq!(tree, same_tree);
        assert!(!clean);
    }

    #[test]
    fn single_command_takes_precedence_over_stages() {
        let stages = stage_map(&[("build", "make")]);
        assert_eq!(
            plan_validation(Some("make test"), &stages, None),
            LocalValidationPlan::SingleCommand("make test".to_owned())
        );
    }

    #[test]
    fn stages_are_returned_in_shipyard_order() {
        let stages = stage_map(&[
            ("test", "ctest"),
            ("setup", "./setup.sh"),
            ("build", "make"),
        ]);
        assert_eq!(
            configured_stages(&stages, None),
            vec![
                StageCommand {
                    stage: "setup".to_owned(),
                    command: "./setup.sh".to_owned()
                },
                StageCommand {
                    stage: "build".to_owned(),
                    command: "make".to_owned()
                },
                StageCommand {
                    stage: "test".to_owned(),
                    command: "ctest".to_owned()
                },
            ]
        );
    }

    #[test]
    fn stages_honor_resume_from() {
        let stages = stage_map(&[
            ("setup", "./setup.sh"),
            ("configure", "cmake -S . -B build"),
            ("build", "make"),
            ("test", "ctest"),
        ]);
        let selected = configured_stages(&stages, Some("build"));
        assert_eq!(
            selected
                .iter()
                .map(|stage| stage.stage.as_str())
                .collect::<Vec<_>>(),
            vec!["build", "test"]
        );
    }

    #[test]
    fn missing_resume_stage_returns_no_later_stages() {
        let stages = stage_map(&[("setup", "./setup.sh"), ("build", "make")]);
        assert!(configured_stages(&stages, Some("test")).is_empty());
    }

    #[test]
    fn empty_config_is_missing() {
        assert_eq!(
            plan_validation(None, &BTreeMap::new(), None),
            LocalValidationPlan::Missing
        );
    }

    #[test]
    fn prepared_state_opt_in_defaults_false() {
        assert!(!prepared_state_enabled(None));
        assert!(!prepared_state_enabled(Some(&BTreeMap::new())));

        let mut section = BTreeMap::new();
        section.insert("enabled".to_owned(), true);
        assert!(prepared_state_enabled(Some(&section)));

        section.insert("enabled".to_owned(), false);
        assert!(!prepared_state_enabled(Some(&section)));
    }

    #[test]
    fn read_log_tail_returns_full_small_file() {
        let temp = tempfile::NamedTempFile::new().expect("file");
        std::fs::write(temp.path(), "hello\nworld\n").expect("write");
        assert_eq!(read_log_tail(temp.path(), 8192), "hello\nworld\n");
    }

    #[test]
    fn read_log_tail_returns_only_tail_for_large_file() {
        let temp = tempfile::NamedTempFile::new().expect("file");
        std::fs::write(temp.path(), "0123456789abcdef").expect("write");
        assert_eq!(read_log_tail(temp.path(), 6), "abcdef");
    }

    #[test]
    fn read_log_tail_tolerates_missing_file() {
        assert_eq!(
            read_log_tail(std::path::Path::new("/definitely/missing/shipyard.log"), 6),
            ""
        );
    }

    fn target() -> LocalTargetConfig {
        LocalTargetConfig {
            name: "test".to_owned(),
            platform: "macos-arm64".to_owned(),
            ..LocalTargetConfig::default()
        }
    }

    fn request<'a>(
        log_path: std::path::PathBuf,
        validation: LocalValidationConfig,
    ) -> LocalValidationRequest<'a> {
        let mut request = LocalValidationRequest::new(log_path, validation);
        request.sha = "abc1234".to_owned();
        request.branch = "test-branch".to_owned();
        request.target = target();
        request
    }

    #[test]
    fn full_execution_marker_rejects_any_resume_request() {
        let repo = tempfile::tempdir().expect("repo");
        init_git_repo(repo.path());
        let validation = LocalValidationConfig {
            command: Some("true".to_owned()),
            ..LocalValidationConfig::default()
        };
        let mut full = request(repo.path().join("full.log"), validation.clone());
        full.target.cwd = Some(repo.path().to_path_buf());
        let full = LocalExecutor::default().validate(full);
        assert_eq!(full.full_execution, Some(true));

        let mut resumed = request(repo.path().join("resumed.log"), validation);
        resumed.target.cwd = Some(repo.path().to_path_buf());
        resumed.resume_from = Some("test".to_owned());
        let resumed = LocalExecutor::default().validate(resumed);
        assert_eq!(resumed.full_execution, Some(false));
    }

    fn contract(markers: &[&str]) -> ContractConfig {
        ContractConfig::new(markers.iter().map(|marker| (*marker).to_owned()).collect())
    }

    fn init_git_repo(path: &Path) {
        std::fs::create_dir_all(path).expect("repo dir");
        git(path, &["init", "-q"]);
        git(path, &["config", "user.email", "shipyard@example.invalid"]);
        git(path, &["config", "user.name", "Shipyard Test"]);
        std::fs::write(path.join("source.txt"), "initial\n").expect("seed file");
        git(path, &["add", "source.txt"]);
        git(path, &["commit", "-qm", "initial"]);
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .expect("git command");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn local_single_command_with_marker_passes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let validation = LocalValidationConfig {
            command: Some("echo __PULP_VALIDATION__:smoke".to_owned()),
            contract: Some(contract(&["__PULP_VALIDATION__:smoke"])),
            ..LocalValidationConfig::default()
        };

        let result =
            LocalExecutor::default().validate(request(temp.path().join("run.log"), validation));

        assert_eq!(result.status, TargetStatus::Pass);
        assert_eq!(
            result.contract_markers_seen,
            vec!["__PULP_VALIDATION__:smoke".to_owned()]
        );
        assert!(result.contract_markers_missing.is_empty());
        assert!(result.contract_violation.is_none());
    }

    #[test]
    fn local_validation_applies_explicit_environment_to_every_stage() {
        let temp = tempfile::tempdir().expect("tempdir");
        let validation = LocalValidationConfig {
            stages: stage_map(&[
                ("configure", "test \"$SHIPYARD_TEST_MACHINE_ENV\" = exact"),
                ("test", "test \"$SHIPYARD_TEST_MACHINE_ENV\" = exact"),
            ]),
            environment: BTreeMap::from([(
                "SHIPYARD_TEST_MACHINE_ENV".to_owned(),
                "exact".to_owned(),
            )]),
            ..LocalValidationConfig::default()
        };

        let result =
            LocalExecutor::default().validate(request(temp.path().join("run.log"), validation));

        assert_eq!(result.status, TargetStatus::Pass);
    }

    #[test]
    fn local_single_command_without_marker_fails_when_enforced() {
        let temp = tempfile::tempdir().expect("tempdir");
        let validation = LocalValidationConfig {
            command: Some("echo no-marker".to_owned()),
            contract: Some(contract(&[
                "__PULP_VALIDATION__:smoke",
                "__PULP_VALIDATION__:full",
            ])),
            ..LocalValidationConfig::default()
        };

        let result =
            LocalExecutor::default().validate(request(temp.path().join("run.log"), validation));

        assert_eq!(result.status, TargetStatus::Fail);
        assert_eq!(result.failure_class.as_deref(), Some("CONTRACT"));
        assert!(
            result
                .contract_violation
                .expect("violation")
                .contains("at least one")
        );
    }

    #[test]
    fn local_single_command_warn_only_contract_preserves_pass() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = contract(&["__PULP_VALIDATION__:smoke"]);
        contract.enforce = false;
        let validation = LocalValidationConfig {
            command: Some("echo no-marker".to_owned()),
            contract: Some(contract),
            ..LocalValidationConfig::default()
        };

        let result =
            LocalExecutor::default().validate(request(temp.path().join("run.log"), validation));

        assert_eq!(result.status, TargetStatus::Pass);
        assert!(result.contract_violation.is_some());
        assert!(result.failure_class.is_none());
    }

    #[test]
    fn local_stages_accumulate_contract_markers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let validation = LocalValidationConfig {
            stages: stage_map(&[
                ("setup", "echo setup"),
                ("configure", "echo __PULP_VALIDATION__:smoke"),
                ("build", "echo build"),
                ("test", "echo test"),
            ]),
            contract: Some(contract(&["__PULP_VALIDATION__:smoke"])),
            ..LocalValidationConfig::default()
        };

        let result =
            LocalExecutor::default().validate(request(temp.path().join("run.log"), validation));

        assert_eq!(result.status, TargetStatus::Pass);
        assert_eq!(result.phase.as_deref(), Some("test"));
        assert_eq!(
            result.contract_markers_seen,
            vec!["__PULP_VALIDATION__:smoke".to_owned()]
        );
    }

    #[test]
    fn local_stage_failure_stops_pipeline_and_classifies_test_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let log_path = temp.path().join("run.log");
        let validation = LocalValidationConfig {
            stages: stage_map(&[
                ("setup", "echo setup"),
                ("build", "echo build && exit 7"),
                ("test", "echo SHOULD_NOT_RUN"),
            ]),
            ..LocalValidationConfig::default()
        };

        let result = LocalExecutor::default().validate(request(log_path.clone(), validation));

        assert_eq!(result.status, TargetStatus::Fail);
        assert_eq!(result.phase.as_deref(), Some("build"));
        assert_eq!(
            result.error_message.as_deref(),
            Some("Stage 'build' failed")
        );
        assert_eq!(result.failure_class.as_deref(), Some("TEST"));
        assert!(
            !std::fs::read_to_string(log_path)
                .expect("log")
                .contains("SHOULD_NOT_RUN")
        );
    }

    #[test]
    fn local_stages_fail_fast_when_tree_drifts_between_stages() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        init_git_repo(&repo);
        let log_path = temp.path().join("run.log");
        let validation = LocalValidationConfig {
            stages: stage_map(&[
                ("setup", "printf changed > source.txt"),
                ("build", "echo SHOULD_NOT_RUN"),
            ]),
            ..LocalValidationConfig::default()
        };
        let mut request = request(log_path.clone(), validation);
        request.target.cwd = Some(repo);

        let result = LocalExecutor::default().validate(request);

        assert_eq!(result.status, TargetStatus::Error);
        assert_eq!(result.phase.as_deref(), Some("build"));
        assert_eq!(result.failure_class.as_deref(), Some("TREE_DRIFT"));
        let message = result.error_message.expect("tree drift message");
        assert!(message.contains("working tree changed during `shipyard run`"));
        assert!(message.contains("stage=build"));
        assert!(message.contains("M source.txt"));
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains("=== TREE_DRIFT at build ==="));
        assert!(!log.contains("SHOULD_NOT_RUN"));
    }

    #[test]
    fn local_stages_allow_tree_drift_escape_hatch_runs_later_stages() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        init_git_repo(&repo);
        let log_path = temp.path().join("run.log");
        let validation = LocalValidationConfig {
            stages: stage_map(&[
                ("setup", "printf changed > source.txt"),
                ("build", "grep changed source.txt && echo RAN_BUILD"),
            ]),
            allow_tree_drift: true,
            ..LocalValidationConfig::default()
        };
        let mut request = request(log_path.clone(), validation);
        request.target.cwd = Some(repo);

        let result = LocalExecutor::default().validate(request);

        assert_eq!(result.status, TargetStatus::Pass);
        assert_eq!(result.phase.as_deref(), Some("build"));
        assert!(
            std::fs::read_to_string(log_path)
                .expect("log")
                .contains("RAN_BUILD")
        );
    }

    #[test]
    fn local_prepared_state_records_and_skips_passed_stages() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = PreparedStateStore::new(temp.path().join("prepared")).expect("store");
        let executor = LocalExecutor::new(Some(store.clone()));
        let stages = stage_map(&[
            ("setup", "echo setup"),
            ("build", "echo build"),
            ("test", "echo test"),
        ]);
        let validation = LocalValidationConfig {
            stages: stages.clone(),
            prepared_state_enabled: true,
            ..LocalValidationConfig::default()
        };

        let first = executor.validate(request(temp.path().join("first.log"), validation.clone()));
        let second = executor.validate(request(temp.path().join("second.log"), validation));

        assert_eq!(first.status, TargetStatus::Pass);
        assert_eq!(second.status, TargetStatus::Pass);
        let record = store.get("abc1234", "test", "default").expect("record");
        assert!(record.is_passed("setup"));
        assert!(record.is_passed("build"));
        assert!(record.is_passed("test"));
        let second_log = std::fs::read_to_string(temp.path().join("second.log")).expect("log");
        assert!(second_log.contains("prepared-state-reuse: skipped"));
        assert!(second_log.contains("setup, build, test"));

        let mut changed_environment = LocalValidationConfig {
            stages,
            prepared_state_enabled: true,
            ..LocalValidationConfig::default()
        };
        changed_environment
            .environment
            .insert("PULP_SDK_DIR".to_owned(), "/new/sdk".to_owned());
        let third_log_path = temp.path().join("third.log");
        let third = executor.validate(request(third_log_path.clone(), changed_environment));
        assert_eq!(third.status, TargetStatus::Pass);
        let third_log = std::fs::read_to_string(third_log_path).expect("log");
        assert!(!third_log.contains("prepared-state-reuse: skipped"));
    }

    #[test]
    fn local_progress_callback_receives_stage_events() {
        let temp = tempfile::tempdir().expect("tempdir");
        let validation = LocalValidationConfig {
            stages: stage_map(&[("build", "echo building")]),
            ..LocalValidationConfig::default()
        };
        let mut request = request(temp.path().join("run.log"), validation);
        let mut phases = Vec::new();
        let result_status = {
            let mut callback = |event: crate::executor::streaming::ProgressEvent| {
                if let Some(phase) = event.phase {
                    phases.push(phase);
                }
                crate::executor::streaming::ProgressAction::Continue
            };
            request.progress_callback = Some(&mut callback);
            LocalExecutor::default().validate(request).status
        };

        assert_eq!(result_status, TargetStatus::Pass);
        assert!(phases.iter().any(|phase| phase == "build"));
    }

    #[test]
    fn local_progress_callback_can_cancel_before_next_stage_starts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let validation = LocalValidationConfig {
            stages: stage_map(&[
                ("setup", "touch setup.marker"),
                ("build", "touch build.marker"),
            ]),
            ..LocalValidationConfig::default()
        };
        let mut request = request(temp.path().join("run.log"), validation);
        request.target.cwd = Some(temp.path().to_path_buf());
        let result = {
            let mut callback = |event: crate::executor::streaming::ProgressEvent| {
                if event.phase.as_deref() == Some("build") {
                    crate::executor::streaming::ProgressAction::Terminate(
                        "cancel before build".to_owned(),
                    )
                } else {
                    crate::executor::streaming::ProgressAction::Continue
                }
            };
            request.progress_callback = Some(&mut callback);
            LocalExecutor::default().validate(request)
        };

        assert_eq!(result.status, TargetStatus::Cancelled);
        assert_eq!(result.phase.as_deref(), Some("build"));
        assert_eq!(result.error_message.as_deref(), Some("cancel before build"));
        assert!(temp.path().join("setup.marker").exists());
        assert!(!temp.path().join("build.marker").exists());
    }
}
