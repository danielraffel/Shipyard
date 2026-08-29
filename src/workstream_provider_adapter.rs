//! Digest-pinnable Subrouter companion for fresh workstream recovery.
//!
//! The stdio side is invoked only through the provider-wrapper snapshot. It
//! creates or reconciles one idempotently named cmux workspace. The launched
//! helper revalidates file-backed routing material and passes the selected
//! account to Subrouter through its bounded environment contract; plaintext is
//! never serialized into the ledger, capsule, argv, response, or logs.

use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::process::run_output_until;
use crate::provider_wrapper::{
    NotAcceptedV1, ProviderAcceptanceV1, ProviderReasoningEffortV1, ProviderTerminalRouteV1,
    ProviderWrapperConfig, ProviderWrapperOperationV1, ProviderWrapperOutcomeV1,
    ProviderWrapperRequestV1, ProviderWrapperResponseV1, UnknownV1, validate_request,
};

const MAX_REQUEST_BYTES: u64 = 64 * 1024;
const MAX_SECRET_BYTES: u64 = 64 * 1024;
const COMMAND_DEADLINE: Duration = Duration::from_secs(15);
const CMUX_APP: &str = "/Applications/cmux.app";
const CMUX_CLI: &str = "/Applications/cmux.app/Contents/Resources/bin/cmux";
const CODESIGN: &str = "/usr/bin/codesign";
const MANAFLOW_TEAM_ID: &str = "7WLXT3NR37";
const COMPANION_PATH_ENV: &str = "SHIPYARD_PROVIDER_COMPANION_PATH";

/// Read one strict protected request and emit exactly one strict response.
pub fn run_stdio() -> Result<(), String> {
    let request = read_request(std::io::stdin().lock())?;
    let response = handle_request(&request, &mut ProductionCmuxRunner);
    let canonical =
        serde_json::to_vec(&response).map_err(|_| "response cannot be serialized".to_owned())?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&canonical)
        .and_then(|()| stdout.write_all(b"\n"))
        .map_err(|_| "response output is unwritable".to_owned())
}

/// Consume one private, reference-only launch capsule from a cmux surface.
pub fn run_launch_capsule(path: &Path) -> Result<(), String> {
    let bytes = read_capsule(path)?;
    let _ = fs::remove_file(path);
    let request: ProviderWrapperRequestV1 = serde_json::from_slice(&bytes)
        .map_err(|_| "launch capsule is not strict v1 JSON".to_owned())?;
    validate_adapter_request(&request)?;
    verify_current_companion(&request)?;
    let account = read_secret_file(
        &request.subrouter_routing.account_file.path,
        &request.subrouter_routing.account_file.sha256,
    )?;
    let account = parse_account(&account)?;
    let native_resume = expected_native_checkpoint(&request)?;
    let headers = read_secret_file(
        &request.subrouter_routing.session_headers_file.path,
        &request.subrouter_routing.session_headers_file.sha256,
    )?;
    let headers: SessionHeadersV1 = serde_json::from_slice(&headers)
        .map_err(|_| "session-header file is not strict v1 JSON".to_owned())?;
    headers.validate()?;
    let subrouter_bytes = read_executable(
        Path::new(&request.subrouter_routing.subrouter_executable_path),
        &request.subrouter_routing.subrouter_executable_sha256,
    )?;
    let agent_bytes = read_executable(
        Path::new(&request.subrouter_routing.agent_executable_path),
        &request.subrouter_routing.agent_executable_sha256,
    )?;
    if request.provider_id != "codex" {
        return Err("only the exact Codex Subrouter adapter is currently supported".to_owned());
    }
    let snapshots = tempfile::Builder::new()
        .prefix("shipyard-provider-executables-")
        .tempdir()
        .map_err(|_| "provider-executable-snapshot-directory-refused".to_owned())?;
    fs::set_permissions(snapshots.path(), fs::Permissions::from_mode(0o700))
        .map_err(|_| "provider-executable-snapshot-directory-refused".to_owned())?;
    let subrouter_path =
        write_executable_snapshot(snapshots.path(), "subrouter", &subrouter_bytes)?;
    let agent_path = write_executable_snapshot(snapshots.path(), "agent", &agent_bytes)?;
    let mut command = Command::new(&subrouter_path);
    command.args([OsStr::new("codex"), OsStr::new("resume")]);
    command.arg(native_resume);
    if let Some(model) = &request.launch_options.model_id {
        command.args(["--model", model]);
    }
    if let Some(effort) = request.launch_options.reasoning_effort {
        command.args([
            "-c",
            &format!("model_reasoning_effort=\"{}\"", effort_name(effort)),
        ]);
    }
    command.arg(resume_prompt(&request));
    command
        .env_clear()
        .env("HOME", headers.home)
        .env("TMPDIR", headers.tmpdir)
        .env("SUBROUTER_CODEX_ACCOUNT_ID", account)
        .env("SUBROUTER_CODEX_BIN", &agent_path);
    if let Some(user_email) = headers.user_email {
        command.env("SUBROUTER_CODEX_USER_EMAIL", user_email);
    }
    let status = command
        .status()
        .map_err(|_| "Subrouter launch was unavailable".to_owned())?;
    if status.success() {
        Ok(())
    } else {
        Err("Subrouter agent exited unsuccessfully".to_owned())
    }
}

fn read_request(input: impl Read) -> Result<ProviderWrapperRequestV1, String> {
    let mut bytes = Vec::new();
    input
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "request input is unreadable".to_owned())?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err("request exceeds the bounded input limit".to_owned());
    }
    serde_json::from_slice(&bytes).map_err(|_| "request is not strict v1 JSON".to_owned())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandResult {
    success: bool,
    stdout: Vec<u8>,
}

trait CmuxRunner {
    fn verify(&mut self) -> Result<(), &'static str>;
    fn run(&mut self, args: &[String]) -> Result<CommandResult, &'static str>;
}

struct ProductionCmuxRunner;

impl CmuxRunner for ProductionCmuxRunner {
    fn verify(&mut self) -> Result<(), &'static str> {
        verify_bundled_cmux()
    }

    fn run(&mut self, args: &[String]) -> Result<CommandResult, &'static str> {
        let mut command = Command::new(CMUX_CLI);
        command.args(args).env_clear();
        let output = run_output_until(
            &mut command,
            Instant::now() + COMMAND_DEADLINE,
            "cmux workstream provider",
        )
        .map_err(|_| "cmux-command-unavailable")?;
        Ok(CommandResult {
            success: output.status.success(),
            stdout: output.stdout,
        })
    }
}

#[allow(clippy::too_many_lines)] // Acceptance ordering is a safety boundary; keep it adjacent.
fn handle_request(
    request: &ProviderWrapperRequestV1,
    runner: &mut impl CmuxRunner,
) -> ProviderWrapperResponseV1 {
    if let Err(code) = validate_adapter_request(request) {
        let outcome = if request.operation == ProviderWrapperOperationV1::Submit {
            rejected(&code)
        } else {
            uncertain(&code)
        };
        return response(request, outcome);
    }
    if !matches!(
        request.subrouter_routing.terminal,
        ProviderTerminalRouteV1::Cmux { .. }
    ) {
        let outcome = if request.operation == ProviderWrapperOperationV1::Submit {
            retryable("terminal-adapter-unsupported")
        } else {
            uncertain("terminal-adapter-unsupported")
        };
        return response(request, outcome);
    }
    if let Err(code) = runner.verify() {
        let outcome = if request.operation == ProviderWrapperOperationV1::Submit {
            retryable(code)
        } else {
            uncertain(code)
        };
        return response(request, outcome);
    }
    let description = format!(
        "shipyard-workstream-delivery:{}",
        request.delivery_fence.idempotency_key
    );
    let listed = match list_matching_workspaces(runner, &description) {
        Ok(listed) => listed,
        Err(code) => {
            let outcome = if request.operation == ProviderWrapperOperationV1::Submit {
                retryable(code)
            } else {
                uncertain(code)
            };
            return response(request, outcome);
        }
    };
    match listed.as_slice() {
        [workspace_id] => {
            return response(
                request,
                reconcile_existing_workspace(request, runner, workspace_id, &description),
            );
        }
        [] => {}
        _ => return response(request, uncertain("multiple-idempotency-workspaces")),
    }
    if request.operation == ProviderWrapperOperationV1::Reconcile {
        return response(request, uncertain("reconcile-workspace-not-yet-visible"));
    }
    if let Err(code) = preflight_live_material(request) {
        return response(request, rejected(&code));
    }
    let capsule = match write_capsule(request) {
        Ok(path) => path,
        Err(code) => return response(request, rejected(&code)),
    };
    let Some(companion) = std::env::var_os(COMPANION_PATH_ENV) else {
        let _ = fs::remove_file(capsule);
        return response(request, rejected("companion-path-unavailable"));
    };
    let companion = PathBuf::from(companion);
    let args = match create_args(request, &description, &capsule, &companion) {
        Ok(args) => args,
        Err(code) => {
            let _ = fs::remove_file(capsule);
            return response(request, rejected(&code));
        }
    };
    if let Err(code) = verify_live_worktree(request) {
        let _ = fs::remove_file(capsule);
        return response(request, rejected(&code));
    }
    let created = match runner.run(&args) {
        Ok(result) if result.success => result,
        Ok(_) | Err(_) => return response(request, uncertain("cmux-create-outcome-unknown")),
    };
    let Ok(created) = parse_created_workspace(&created.stdout) else {
        return response(request, uncertain("cmux-create-response-invalid"));
    };
    let expected_checkpoint = match expected_native_checkpoint(request) {
        Ok(value) => value,
        Err(code) => return response(request, uncertain(&code)),
    };
    response(
        request,
        match session_binding_for_surface(
            runner,
            &created.workspace_id,
            &created.surface_id,
            &request.provider_id,
        ) {
            Ok(Some(binding)) if binding.checkpoint_id == expected_checkpoint => {
                delivered(request, &created.workspace_id, &description, &binding)
            }
            Ok(Some(_)) => uncertain("cmux-session-evidence-resume-mismatch"),
            Ok(None) => uncertain("cmux-session-binding-not-yet-visible"),
            Err(code) => uncertain(code),
        },
    )
}

fn reconcile_existing_workspace(
    request: &ProviderWrapperRequestV1,
    runner: &mut impl CmuxRunner,
    workspace_id: &str,
    description: &str,
) -> ProviderWrapperOutcomeV1 {
    let expected_checkpoint = match expected_native_checkpoint(request) {
        Ok(value) => value,
        Err(code) => return uncertain(&code),
    };
    match session_bindings_for_workspace(runner, workspace_id, &request.provider_id) {
        Ok(bindings) if bindings.len() == 1 && bindings[0].checkpoint_id == expected_checkpoint => {
            delivered(request, workspace_id, description, &bindings[0])
        }
        Ok(bindings) if bindings.is_empty() => uncertain("cmux-session-binding-not-yet-visible"),
        Ok(bindings) if bindings.len() == 1 => uncertain("cmux-session-evidence-resume-mismatch"),
        Ok(_) => uncertain("multiple-provider-session-bindings"),
        Err(code) => uncertain(code),
    }
}

fn expected_native_checkpoint(request: &ProviderWrapperRequestV1) -> Result<String, String> {
    canonical_uuid(&request.subrouter_routing.native_resume_id)
        .ok_or_else(|| "native-resume-checkpoint-invalid".to_owned())
}

fn validate_adapter_request(request: &ProviderWrapperRequestV1) -> Result<(), String> {
    if request.adapter_id != "subrouter" || request.provider_id != "codex" {
        return Err("unsupported-provider-or-adapter".to_owned());
    }
    let config = ProviderWrapperConfig {
        executable_path: PathBuf::from("/adapter-validation-only"),
        executable_sha256: request.subrouter_routing.companion_sha256.clone(),
        provider_id: request.provider_id.clone(),
        adapter_id: request.adapter_id.clone(),
        deadline_seconds: 1,
        max_stdout_bytes: 1,
        max_stderr_bytes: 1,
    };
    validate_request(&config, request).map_err(|_| "invalid-provider-request".to_owned())
}

fn preflight_live_material(request: &ProviderWrapperRequestV1) -> Result<(), String> {
    let companion = std::env::var_os(COMPANION_PATH_ENV)
        .ok_or_else(|| "companion-path-unavailable".to_owned())?;
    let companion = PathBuf::from(companion);
    verify_executable(&companion, &request.subrouter_routing.companion_sha256)?;
    read_secret_file(
        &request.subrouter_routing.account_file.path,
        &request.subrouter_routing.account_file.sha256,
    )?;
    read_secret_file(
        &request.subrouter_routing.session_headers_file.path,
        &request.subrouter_routing.session_headers_file.sha256,
    )?;
    verify_executable(
        Path::new(&request.subrouter_routing.subrouter_executable_path),
        &request.subrouter_routing.subrouter_executable_sha256,
    )?;
    verify_executable(
        Path::new(&request.subrouter_routing.agent_executable_path),
        &request.subrouter_routing.agent_executable_sha256,
    )
}

fn verify_live_worktree(request: &ProviderWrapperRequestV1) -> Result<(), String> {
    let path = Path::new(&request.resume_expectation.worktree_path);
    let metadata = fs::symlink_metadata(path).map_err(|_| "worktree-unavailable".to_owned())?;
    if !path.is_absolute()
        || path.components().collect::<PathBuf>() != path
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
        || !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err("worktree-identity-refused".to_owned());
    }
    let head = git_output(path, &["rev-parse", "--verify", "HEAD"])?;
    if head != request.resume_expectation.head_sha {
        return Err("worktree-head-drifted".to_owned());
    }
    let remote = git_output(path, &["remote", "get-url", "origin"])?;
    let expected = &request.resume_expectation.repository;
    let accepted = [
        format!("git@github.com:{expected}.git"),
        format!("https://github.com/{expected}.git"),
        format!("https://github.com/{expected}"),
    ];
    if !accepted.iter().any(|candidate| candidate == &remote) {
        return Err("worktree-repository-mismatch".to_owned());
    }
    Ok(())
}

fn git_output(path: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("/usr/bin/git");
    command.arg("-C").arg(path).args(args).env_clear();
    let output = run_output_until(
        &mut command,
        Instant::now() + COMMAND_DEADLINE,
        "workstream repository verification",
    )
    .map_err(|_| "worktree-git-unavailable".to_owned())?;
    if !output.status.success() || output.stdout.len() > 4096 {
        return Err("worktree-git-refused".to_owned());
    }
    let value = std::str::from_utf8(&output.stdout)
        .map_err(|_| "worktree-git-output-invalid".to_owned())?
        .trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.contains(['\r', '\n', '\0']) {
        return Err("worktree-git-output-invalid".to_owned());
    }
    Ok(value.to_owned())
}

fn write_capsule(request: &ProviderWrapperRequestV1) -> Result<PathBuf, String> {
    let secret_path = Path::new(&request.subrouter_routing.account_file.path);
    let directory = secret_path
        .parent()
        .ok_or_else(|| "secret-parent-unavailable".to_owned())?;
    verify_secret_directory(directory)?;
    let mut capsule = tempfile::Builder::new()
        .prefix(".shipyard-workstream-")
        .suffix(".json")
        .tempfile_in(directory)
        .map_err(|_| "launch-capsule-create-refused".to_owned())?;
    capsule
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .and_then(|()| {
            serde_json::to_writer(capsule.as_file_mut(), request).map_err(std::io::Error::other)
        })
        .and_then(|()| capsule.as_file().sync_all())
        .map_err(|_| "launch-capsule-write-refused".to_owned())?;
    let (_file, path) = capsule
        .keep()
        .map_err(|_| "launch-capsule-persist-refused".to_owned())?;
    Ok(path)
}

fn read_capsule(path: &Path) -> Result<Vec<u8>, String> {
    let approved = approved_secret_directory()?;
    if path.parent() != Some(approved.as_path()) {
        return Err("launch-capsule-outside-approved-directory".to_owned());
    }
    verify_secret_directory(&approved)?;
    read_pinned_file(path, None, MAX_REQUEST_BYTES, 0o600, false)
}

fn read_secret_file(path: &str, expected_digest: &str) -> Result<Vec<u8>, String> {
    let path = Path::new(path);
    let approved = approved_secret_directory()?;
    if path.parent() != Some(approved.as_path()) {
        return Err("secret-path-outside-approved-directory".to_owned());
    }
    verify_secret_directory(&approved)?;
    read_pinned_file(path, Some(expected_digest), MAX_SECRET_BYTES, 0o600, false)
}

fn approved_secret_directory() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME-unavailable".to_owned())?;
    Ok(PathBuf::from(home).join(".config/pulp/secrets"))
}

fn verify_secret_directory(path: &Path) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| "secret-directory-unavailable".to_owned())?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err("secret-directory-permissions-refused".to_owned());
    }
    Ok(())
}

fn verify_executable(path: &Path, expected_digest: &str) -> Result<(), String> {
    read_executable(path, expected_digest).map(drop)
}

fn read_executable(path: &Path, expected_digest: &str) -> Result<Vec<u8>, String> {
    read_pinned_file(path, Some(expected_digest), 128 * 1024 * 1024, 0, true)
}

fn write_executable_snapshot(
    directory: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let path = directory.join(name);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(&path)
        .map_err(|_| "provider-executable-snapshot-create-refused".to_owned())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| "provider-executable-snapshot-write-refused".to_owned())?;
    Ok(path)
}

fn verify_current_companion(request: &ProviderWrapperRequestV1) -> Result<(), String> {
    let current =
        std::env::current_exe().map_err(|_| "companion-identity-unavailable".to_owned())?;
    verify_executable(&current, &request.subrouter_routing.companion_sha256)
}

fn read_pinned_file(
    path: &Path,
    expected_digest: Option<&str>,
    max_bytes: u64,
    exact_mode: u32,
    executable: bool,
) -> Result<Vec<u8>, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || path.components().collect::<PathBuf>() != path
    {
        return Err("file-reference-is-not-canonical".to_owned());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .map_err(|_| "file-reference-open-refused".to_owned())?;
    let before = file
        .metadata()
        .map_err(|_| "file-reference-metadata-refused".to_owned())?;
    let mode = before.permissions().mode() & 0o777;
    if !before.is_file()
        || before.uid() != nix::unistd::Uid::effective().as_raw()
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > max_bytes
        || (exact_mode != 0 && mode != exact_mode)
        || (executable && (mode & 0o111 == 0 || mode & 0o022 != 0))
    {
        return Err("file-reference-metadata-refused".to_owned());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "file-reference-read-refused".to_owned())?;
    let after = file
        .metadata()
        .map_err(|_| "file-reference-metadata-refused".to_owned())?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || bytes.len() as u64 != before.len()
    {
        return Err("file-reference-drift-refused".to_owned());
    }
    if expected_digest.is_some_and(|expected| hex::encode(Sha256::digest(&bytes)) != expected) {
        return Err("file-reference-digest-refused".to_owned());
    }
    Ok(bytes)
}

fn parse_account(bytes: &[u8]) -> Result<OsString, String> {
    parse_bounded_token(bytes, "account")
}

fn parse_bounded_token(bytes: &[u8], kind: &str) -> Result<OsString, String> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| format!("{kind}-file-is-not-UTF-8"))?
        .trim_end_matches(['\r', '\n']);
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(char::is_control)
        || value.chars().any(char::is_whitespace)
    {
        return Err(format!("{kind}-file-value-refused"));
    }
    Ok(OsString::from(value))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionHeadersV1 {
    schema_version: u32,
    home: String,
    tmpdir: String,
    user_email: Option<String>,
}

impl SessionHeadersV1 {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1
            || !Path::new(&self.home).is_absolute()
            || !Path::new(&self.tmpdir).is_absolute()
            || self.user_email.as_ref().is_some_and(|value| {
                value.is_empty() || value.len() > 320 || value.contains(char::is_whitespace)
            })
        {
            return Err("session-header-values-refused".to_owned());
        }
        Ok(())
    }
}

fn create_args(
    request: &ProviderWrapperRequestV1,
    description: &str,
    capsule: &Path,
    companion: &Path,
) -> Result<Vec<String>, String> {
    let command = format!(
        "{} --launch-capsule {}",
        shell_word(&companion.to_string_lossy())?,
        shell_word(&capsule.to_string_lossy())?
    );
    let mut args = cmux_prefix(["workspace", "create"]);
    args.extend([
        "--name".to_owned(),
        format!(
            "{} — tracked workstream",
            request.resume_expectation.workstream_handle
        ),
        "--description".to_owned(),
        description.to_owned(),
        "--cwd".to_owned(),
        request.resume_expectation.worktree_path.clone(),
        "--focus".to_owned(),
        "false".to_owned(),
        "--command".to_owned(),
        command,
    ]);
    Ok(args)
}

fn list_matching_workspaces(
    runner: &mut impl CmuxRunner,
    description: &str,
) -> Result<Vec<String>, &'static str> {
    let windows = runner.run(&cmux_prefix(["list-windows"]))?;
    if !windows.success {
        return Err("cmux-window-list-refused");
    }
    let windows: Vec<ListedWindow> =
        serde_json::from_slice(&windows.stdout).map_err(|_| "cmux-window-list-response-invalid")?;
    if windows.is_empty() {
        return Err("cmux-window-list-empty");
    }
    let mut matches = Vec::new();
    for window in windows {
        let window_id = canonical_uuid(&window.id).ok_or("cmux-window-id-invalid")?;
        let mut args = cmux_prefix(["workspace", "list"]);
        args.extend(["--window".to_owned(), window_id.clone()]);
        let listed = runner.run(&args)?;
        if !listed.success {
            return Err("cmux-workspace-list-refused");
        }
        let listed: WorkspaceList = serde_json::from_slice(&listed.stdout)
            .map_err(|_| "cmux-workspace-list-response-invalid")?;
        if canonical_uuid(&listed.window_id).as_deref() != Some(window_id.as_str()) {
            return Err("cmux-workspace-list-window-mismatch");
        }
        for workspace in listed.workspaces {
            if workspace.description.as_deref() == Some(description) {
                matches
                    .push(canonical_uuid(&workspace.id).ok_or("cmux-list-workspace-id-invalid")?);
            }
        }
    }
    matches.sort();
    if matches.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("cmux-workspace-id-duplicated");
    }
    Ok(matches)
}

fn verify_bundled_cmux() -> Result<(), &'static str> {
    #[cfg(not(target_os = "macos"))]
    return Err("cmux-platform-unsupported");
    #[cfg(target_os = "macos")]
    {
        for path in [Path::new(CMUX_APP), Path::new(CMUX_CLI)] {
            let metadata = fs::metadata(path).map_err(|_| "cmux-unavailable")?;
            if metadata.uid() != nix::unistd::Uid::effective().as_raw()
                || metadata.permissions().mode() & 0o022 != 0
            {
                return Err("cmux-untrusted");
            }
            let requirement = format!(
                "=anchor apple generic and certificate leaf[subject.OU] = \"{MANAFLOW_TEAM_ID}\""
            );
            let output = Command::new(CODESIGN)
                .args([
                    OsStr::new("--verify"),
                    OsStr::new("--strict"),
                    OsStr::new("-R"),
                ])
                .arg(requirement)
                .arg(path)
                .output()
                .map_err(|_| "cmux-unavailable")?;
            if !output.status.success() {
                return Err("cmux-untrusted");
            }
        }
        Ok(())
    }
}

fn resume_prompt(request: &ProviderWrapperRequestV1) -> String {
    format!(
        "Resume tracked workstream {} from its exact protected checkpoint. First acknowledge reconstructed context for wake {} through Shipyard, then continue the durable plan and return ownership with reviewed evidence. Missing Subrouter or terminal provenance must fail closed.",
        request.resume_expectation.workstream_handle, request.delivery_fence.wake_id
    )
}

#[derive(Deserialize)]
struct SurfaceHealth {
    workspace_id: String,
    surfaces: Vec<SurfaceHealthEntry>,
}

#[derive(Deserialize)]
struct SurfaceHealthEntry {
    id: String,
    #[serde(rename = "type")]
    surface_type: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct AgentSessionBinding {
    checkpoint_id: String,
    kind: String,
    source: String,
}

#[derive(Deserialize)]
struct SurfaceResumeEvidence {
    workspace_id: String,
    surface_id: String,
    resume_binding: Option<AgentSessionBinding>,
}

fn session_bindings_for_workspace(
    runner: &mut impl CmuxRunner,
    workspace_id: &str,
    provider_id: &str,
) -> Result<Vec<AgentSessionBinding>, &'static str> {
    let mut args = cmux_prefix(["surface-health"]);
    args.extend(["--workspace".to_owned(), workspace_id.to_owned()]);
    let result = runner.run(&args)?;
    if !result.success {
        return Err("cmux-surface-list-refused");
    }
    let health: SurfaceHealth =
        serde_json::from_slice(&result.stdout).map_err(|_| "cmux-surface-list-response-invalid")?;
    if canonical_uuid(&health.workspace_id).as_deref() != Some(workspace_id) {
        return Err("cmux-surface-list-workspace-mismatch");
    }
    let mut surface_ids = health
        .surfaces
        .into_iter()
        .filter(|surface| surface.surface_type == "terminal")
        .map(|surface| canonical_uuid(&surface.id).ok_or("cmux-surface-id-invalid"))
        .collect::<Result<Vec<_>, _>>()?;
    surface_ids.sort();
    if surface_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("cmux-surface-id-duplicated");
    }
    let mut bindings = Vec::new();
    for surface_id in surface_ids {
        if let Some(binding) =
            session_binding_for_surface(runner, workspace_id, &surface_id, provider_id)?
        {
            bindings.push(binding);
        }
    }
    Ok(bindings)
}

fn session_binding_for_surface(
    runner: &mut impl CmuxRunner,
    workspace_id: &str,
    surface_id: &str,
    provider_id: &str,
) -> Result<Option<AgentSessionBinding>, &'static str> {
    let mut args = cmux_prefix(["surface", "resume", "show"]);
    args.extend([
        "--workspace".to_owned(),
        workspace_id.to_owned(),
        "--surface".to_owned(),
        surface_id.to_owned(),
    ]);
    let result = runner.run(&args)?;
    if !result.success {
        return Err("cmux-session-evidence-refused");
    }
    let evidence: SurfaceResumeEvidence = serde_json::from_slice(&result.stdout)
        .map_err(|_| "cmux-session-evidence-response-invalid")?;
    if canonical_uuid(&evidence.workspace_id).as_deref() != Some(workspace_id)
        || canonical_uuid(&evidence.surface_id).as_deref() != Some(surface_id)
    {
        return Err("cmux-session-evidence-target-mismatch");
    }
    let Some(mut binding) = evidence.resume_binding else {
        return Ok(None);
    };
    if binding.kind != provider_id || binding.source != "agent-hook" {
        return Err("cmux-session-evidence-provider-mismatch");
    }
    binding.checkpoint_id =
        canonical_uuid(&binding.checkpoint_id).ok_or("cmux-session-evidence-checkpoint-invalid")?;
    Ok(Some(binding))
}

fn delivered(
    request: &ProviderWrapperRequestV1,
    workspace_id: &str,
    description: &str,
    binding: &AgentSessionBinding,
) -> ProviderWrapperOutcomeV1 {
    #[derive(Serialize)]
    struct Receipt<'a> {
        domain: &'static str,
        provider_id: &'a str,
        idempotency_key: &'a str,
        workspace_id: &'a str,
        description: &'a str,
        routing_generation: u64,
        launch_generation: u64,
        agent_adapter_generation: u64,
        account_ref: &'a str,
        model_ref: &'a str,
        session_headers_ref: &'a str,
        native_session_ref: &'a str,
        native_resume_ref: &'a str,
        native_resume_id: &'a str,
        server_ref: &'a str,
        provider_route_ref: &'a str,
        account_file_sha256: &'a str,
        session_headers_file_sha256: &'a str,
        companion_sha256: &'a str,
        subrouter_executable_sha256: &'a str,
        agent_executable_sha256: &'a str,
        session_checkpoint_id: &'a str,
    }
    let route = &request.subrouter_routing;
    let bytes = serde_json::to_vec(&Receipt {
        domain: "shipyard-subrouter-provider-receipt-v1",
        provider_id: &request.provider_id,
        idempotency_key: &request.delivery_fence.idempotency_key,
        workspace_id,
        description,
        routing_generation: route.routing_generation,
        launch_generation: route.launch_generation,
        agent_adapter_generation: route.agent_adapter_generation,
        account_ref: &route.account_ref,
        model_ref: &route.model_ref,
        session_headers_ref: &route.session_headers_ref,
        native_session_ref: &route.native_session_ref,
        native_resume_ref: &route.native_resume_ref,
        native_resume_id: &route.native_resume_id,
        server_ref: &route.server_ref,
        provider_route_ref: &route.provider_route_ref,
        account_file_sha256: &route.account_file.sha256,
        session_headers_file_sha256: &route.session_headers_file.sha256,
        companion_sha256: &route.companion_sha256,
        subrouter_executable_sha256: &route.subrouter_executable_sha256,
        agent_executable_sha256: &route.agent_executable_sha256,
        session_checkpoint_id: &binding.checkpoint_id,
    })
    .expect("fixed receipt serialization cannot fail");
    ProviderWrapperOutcomeV1::Delivered {
        acceptance: ProviderAcceptanceV1::ProviderSessionAccepted,
        provider_session_ref: format!("session:{}:{}", request.provider_id, binding.checkpoint_id),
        receipt_digest: hex::encode(Sha256::digest(bytes)),
    }
}

fn response(
    request: &ProviderWrapperRequestV1,
    outcome: ProviderWrapperOutcomeV1,
) -> ProviderWrapperResponseV1 {
    ProviderWrapperResponseV1 {
        schema_version: 1,
        operation: request.operation,
        provider_id: request.provider_id.clone(),
        adapter_id: request.adapter_id.clone(),
        idempotency_key: request.delivery_fence.idempotency_key.clone(),
        outcome,
    }
}

fn retryable(code: &str) -> ProviderWrapperOutcomeV1 {
    ProviderWrapperOutcomeV1::Retryable {
        launch_state: NotAcceptedV1::NotAccepted,
        error_digest: evidence_digest("retryable", code),
    }
}

fn rejected(code: &str) -> ProviderWrapperOutcomeV1 {
    ProviderWrapperOutcomeV1::Rejected {
        launch_state: NotAcceptedV1::NotAccepted,
        error_digest: evidence_digest("rejected", code),
    }
}

fn uncertain(code: &str) -> ProviderWrapperOutcomeV1 {
    ProviderWrapperOutcomeV1::Uncertain {
        launch_state: UnknownV1::Unknown,
        evidence_digest: evidence_digest("uncertain", code),
    }
}

fn evidence_digest(class: &str, code: &str) -> String {
    hex::encode(Sha256::digest(
        format!("shipyard-subrouter-provider-{class}-v1\0{code}").as_bytes(),
    ))
}

fn cmux_prefix<const N: usize>(tail: [&str; N]) -> Vec<String> {
    ["--json", "--id-format", "uuids"]
        .into_iter()
        .chain(tail)
        .map(str::to_owned)
        .collect()
}

fn shell_word(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value
            .chars()
            .any(|character| character == '\0' || character.is_control())
    {
        return Err("launch-path-is-not-shell-safe".to_owned());
    }
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

const fn effort_name(effort: ProviderReasoningEffortV1) -> &'static str {
    match effort {
        ProviderReasoningEffortV1::Low => "low",
        ProviderReasoningEffortV1::Medium => "medium",
        ProviderReasoningEffortV1::High => "high",
        ProviderReasoningEffortV1::Xhigh => "xhigh",
        ProviderReasoningEffortV1::Max => "max",
        ProviderReasoningEffortV1::Ultra => "ultra",
    }
}

#[derive(Deserialize)]
struct ListedWindow {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceList {
    window_id: String,
    workspaces: Vec<ListedWorkspace>,
}

#[derive(Deserialize)]
struct ListedWorkspace {
    id: String,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatedWorkspace {
    workspace_id: String,
    surface_id: String,
}

struct CreatedWorkspaceIds {
    workspace_id: String,
    surface_id: String,
}

fn parse_created_workspace(bytes: &[u8]) -> Result<CreatedWorkspaceIds, ()> {
    let created: CreatedWorkspace = serde_json::from_slice(bytes).map_err(|_| ())?;
    Ok(CreatedWorkspaceIds {
        workspace_id: canonical_uuid(&created.workspace_id).ok_or(())?,
        surface_id: canonical_uuid(&created.surface_id).ok_or(())?,
    })
}

fn canonical_uuid(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || ![8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| ![8, 13, 18, 23].contains(&index) && !byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests;
