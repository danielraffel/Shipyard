//! Digest-pinnable cmux adapter for one fresh workstream continuation session.
//!
//! The adapter owns no delivery or lifecycle state. It maps one strict wrapper
//! request to one strict response while using the delivery idempotency key as
//! cmux's durable lookup surface. Cargo registers the binary, but the current
//! release workflows and installer still package only `shipyard`; a production
//! follow-up must build, sign, install, and digest-pin this second executable.

#[cfg(target_os = "macos")]
use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{Read, Write};
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::process::run_output_until;
use crate::provider_wrapper::{
    CmuxEndpointV1, NotAcceptedV1, ProviderAcceptanceV1, ProviderWrapperOperationV1,
    ProviderWrapperOutcomeV1, ProviderWrapperRequestV1, ProviderWrapperResponseV1, UnknownV1,
    validate_request,
};
use crate::workstream_continuation_config::ProviderWrapperConfig;

const SCHEMA_VERSION: u32 = 1;
const ADAPTER_ID: &str = "cmux-workstream-v1";
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
#[cfg(target_os = "macos")]
const CODESIGN: &str = "/usr/bin/codesign";
#[cfg(target_os = "macos")]
const MANAFLOW_TEAM_ID: &str = "7WLXT3NR37";
const COMMAND_DEADLINE: Duration = Duration::from_secs(15);

/// Read one strict request from stdin and emit exactly one strict response.
pub fn run_stdio() -> Result<(), String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "request input is unreadable".to_owned())?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err("request exceeds the bounded input limit".to_owned());
    }
    let request: ProviderWrapperRequestV1 =
        serde_json::from_slice(&bytes).map_err(|_| "request is not strict v1 JSON".to_owned())?;
    let response = handle_request(&request, &mut ProductionCmuxRunner::default());
    let canonical =
        serde_json::to_vec(&response).map_err(|_| "response cannot be serialized".to_owned())?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&canonical)
        .and_then(|()| stdout.write_all(b"\n"))
        .map_err(|_| "response output is unwritable".to_owned())
}

/// Verify that this process is the exact companion executable authorized by
/// the controller before serving an auxiliary read-only protocol.
#[cfg(unix)]
pub(crate) fn verify_current_companion_digest(
    expected_digest: &crate::parallel_proof::Sha256Digest,
) -> Result<(), String> {
    const MAX_COMPANION_BYTES: u64 = 128 * 1024 * 1024;

    let current =
        std::env::current_exe().map_err(|_| "companion-identity-unavailable".to_owned())?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(current)
        .map_err(|_| "companion-open-refused".to_owned())?;
    let before = file
        .metadata()
        .map_err(|_| "companion-metadata-refused".to_owned())?;
    if !before.is_file()
        || before.uid() != nix::unistd::Uid::effective().as_raw()
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > MAX_COMPANION_BYTES
        || before.mode() & 0o111 == 0
        || before.mode() & 0o022 != 0
    {
        return Err("companion-metadata-refused".to_owned());
    }
    let mut hasher = Sha256::new();
    let copied = std::io::copy(&mut file, &mut HashWriter(&mut hasher))
        .map_err(|_| "companion-read-refused".to_owned())?;
    let after = file
        .metadata()
        .map_err(|_| "companion-metadata-refused".to_owned())?;
    if copied != before.len()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || hex::encode(hasher.finalize()) != expected_digest.as_str()
    {
        return Err("companion-digest-refused".to_owned());
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn verify_current_companion_digest(
    _expected_digest: &crate::parallel_proof::Sha256Digest,
) -> Result<(), String> {
    Err("companion-digest-verification-unavailable".to_owned())
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandResult {
    success: bool,
    stdout: Vec<u8>,
}

trait CmuxRunner {
    fn bind(&mut self, endpoint: &CmuxEndpointV1) -> Result<(), RunnerFailure>;
    fn run(&mut self, args: &[String]) -> Result<CommandResult, RunnerFailure>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunnerFailure {
    Unavailable,
    #[cfg(any(target_os = "macos", test))]
    Untrusted,
}

#[derive(Default)]
struct ProductionCmuxRunner {
    endpoint: Option<CmuxEndpointV1>,
}

impl CmuxRunner for ProductionCmuxRunner {
    fn bind(&mut self, endpoint: &CmuxEndpointV1) -> Result<(), RunnerFailure> {
        verify_authorized_cmux(endpoint)?;
        self.endpoint = Some(endpoint.clone());
        Ok(())
    }

    fn run(&mut self, args: &[String]) -> Result<CommandResult, RunnerFailure> {
        let endpoint = self.endpoint.as_ref().ok_or(RunnerFailure::Unavailable)?;
        let mut command = Command::new(&endpoint.executable_path);
        command
            .args(["--socket", &endpoint.socket_path])
            .args(args)
            .env_clear();
        let output = run_output_until(
            &mut command,
            Instant::now() + COMMAND_DEADLINE,
            "cmux workstream provider",
        )
        .map_err(|_| RunnerFailure::Unavailable)?;
        Ok(CommandResult {
            success: output.status.success(),
            stdout: output.stdout,
        })
    }
}

#[cfg(target_os = "macos")]
fn verify_authorized_cmux(endpoint: &CmuxEndpointV1) -> Result<(), RunnerFailure> {
    use std::os::unix::fs::FileTypeExt;

    let cli = Path::new(&endpoint.executable_path);
    let socket = Path::new(&endpoint.socket_path);
    let cli_metadata = fs::metadata(cli).map_err(|_| RunnerFailure::Unavailable)?;
    let socket_metadata = fs::symlink_metadata(socket).map_err(|_| RunnerFailure::Unavailable)?;
    let effective_uid = nix::unistd::Uid::effective().as_raw();
    if !cli.is_absolute()
        || !socket.is_absolute()
        || !cli_metadata.is_file()
        || (cli_metadata.uid() != 0 && cli_metadata.uid() != effective_uid)
        || cli_metadata.permissions().mode() & 0o022 != 0
        || cli_metadata.permissions().mode() & 0o111 == 0
        || !socket_metadata.file_type().is_socket()
        || (socket_metadata.uid() != 0 && socket_metadata.uid() != effective_uid)
        || socket_metadata.permissions().mode() & 0o022 != 0
    {
        return Err(RunnerFailure::Untrusted);
    }
    let requirement =
        format!("=anchor apple generic and certificate leaf[subject.OU] = \"{MANAFLOW_TEAM_ID}\"");
    let output = Command::new(CODESIGN)
        .args([
            OsStr::new("--verify"),
            OsStr::new("--strict"),
            OsStr::new("-R"),
        ])
        .arg(requirement)
        .arg(cli)
        .output()
        .map_err(|_| RunnerFailure::Unavailable)?;
    if !output.status.success() {
        return Err(RunnerFailure::Untrusted);
    }
    // Darwin cannot execute a previously verified descriptor. The remaining
    // path race is inside Shipyard's explicit trusted-same-UID boundary, the
    // same authority that owns cmux.app and Shipyard's machine policy.
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn verify_authorized_cmux(_endpoint: &CmuxEndpointV1) -> Result<(), RunnerFailure> {
    Err(RunnerFailure::Unavailable)
}

fn handle_request(
    request: &ProviderWrapperRequestV1,
    runner: &mut impl CmuxRunner,
) -> ProviderWrapperResponseV1 {
    if let Err(code) = validate_adapter_request(request) {
        let outcome = match request.operation {
            ProviderWrapperOperationV1::Submit => rejected(code),
            ProviderWrapperOperationV1::Reconcile => uncertain(code),
        };
        return response(request, outcome);
    }
    match runner.bind(&request.cmux_endpoint) {
        #[cfg(any(target_os = "macos", test))]
        Err(RunnerFailure::Untrusted) => {
            let outcome = match request.operation {
                ProviderWrapperOperationV1::Submit => rejected("cmux-untrusted"),
                ProviderWrapperOperationV1::Reconcile => uncertain("cmux-untrusted"),
            };
            return response(request, outcome);
        }
        Err(RunnerFailure::Unavailable) => {
            let outcome = match request.operation {
                ProviderWrapperOperationV1::Submit => retryable("cmux-unavailable-before-create"),
                ProviderWrapperOperationV1::Reconcile => {
                    uncertain("cmux-unavailable-during-reconcile")
                }
            };
            return response(request, outcome);
        }
        Ok(()) => {}
    }
    let description = format!(
        "shipyard-workstream-delivery:{}",
        request.delivery_fence.idempotency_key
    );
    let listed = match list_matching_workspaces(runner, &description) {
        Ok(listed) => listed,
        Err(code) => {
            let outcome = match request.operation {
                ProviderWrapperOperationV1::Submit => retryable(code),
                ProviderWrapperOperationV1::Reconcile => uncertain(code),
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
        return response(request, uncertain("reconcile-visibility-not-yet-proven"));
    }

    let (args, _private_launch) = match create_args(request, &description) {
        Ok(prepared) => prepared,
        Err(code) => return response(request, rejected(code)),
    };
    // cmux creates the workspace before it sends `--command` to the surface.
    // From this invocation onward every failure is an ambiguous acceptance.
    let created = match runner.run(&args) {
        Ok(result) if result.success => result,
        Ok(_) | Err(_) => return response(request, uncertain("cmux-create-outcome-unknown")),
    };
    let Ok(created) = parse_created_workspace(&created.stdout) else {
        return response(request, uncertain("cmux-create-response-invalid"));
    };
    response(
        request,
        match session_binding_for_surface(
            runner,
            &created.workspace_id,
            &created.surface_id,
            &request.provider_id,
        ) {
            Ok(Some(binding)) => delivered(request, &created.workspace_id, &description, &binding),
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
    match session_bindings_for_workspace(runner, workspace_id, &request.provider_id) {
        Ok(bindings) if bindings.len() == 1 => {
            delivered(request, workspace_id, description, &bindings[0])
        }
        Ok(bindings) if bindings.is_empty() => uncertain("cmux-session-binding-not-yet-visible"),
        Ok(_) => uncertain("multiple-provider-session-bindings"),
        Err(code) => uncertain(code),
    }
}

fn validate_adapter_request(request: &ProviderWrapperRequestV1) -> Result<(), &'static str> {
    if request.adapter_id != ADAPTER_ID {
        return Err("unsupported-provider-or-adapter");
    }
    let config = ProviderWrapperConfig {
        executable_path: PathBuf::from("/adapter-validation-only"),
        executable_sha256: "0".repeat(64),
        provider_id: request.provider_id.clone(),
        adapter_id: ADAPTER_ID.to_owned(),
        deadline_seconds: 1,
        max_stdout_bytes: 1,
        max_stderr_bytes: 1,
    };
    validate_request(&config, request).map_err(|_| "invalid-provider-request")?;
    Ok(())
}

fn list_matching_workspaces(
    runner: &mut impl CmuxRunner,
    description: &str,
) -> Result<Vec<String>, &'static str> {
    let windows_result = runner
        .run(&cmux_prefix(["list-windows"]))
        .map_err(|_| "cmux-window-list-unavailable")?;
    if !windows_result.success {
        return Err("cmux-window-list-refused");
    }
    let mut windows: Vec<ListedWindow> = serde_json::from_slice(&windows_result.stdout)
        .map_err(|_| "cmux-window-list-response-invalid")?;
    if windows.is_empty() {
        return Err("cmux-window-list-empty");
    }
    let mut window_ids = Vec::with_capacity(windows.len());
    for window in windows.drain(..) {
        window_ids.push(canonical_uuid(&window.id).ok_or("cmux-window-id-invalid")?);
    }
    window_ids.sort();
    if window_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("cmux-window-id-duplicated");
    }

    let mut matches = Vec::new();
    for window_id in window_ids {
        let mut args = cmux_prefix(["workspace", "list"]);
        args.extend(["--window".to_owned(), window_id.clone()]);
        let result = runner
            .run(&args)
            .map_err(|_| "cmux-workspace-list-unavailable")?;
        if !result.success {
            return Err("cmux-workspace-list-refused");
        }
        let listed: WorkspaceList = serde_json::from_slice(&result.stdout)
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
    Ok(matches)
}

#[derive(Deserialize)]
struct ListedWindow {
    id: String,
}

fn cmux_prefix<const N: usize>(tail: [&str; N]) -> Vec<String> {
    ["--json", "--id-format", "uuids"]
        .into_iter()
        .chain(tail)
        .map(str::to_owned)
        .collect()
}

fn create_args(
    request: &ProviderWrapperRequestV1,
    description: &str,
) -> Result<(Vec<String>, PrivateLaunch), &'static str> {
    let private_launch = prepare_private_launch(request)?;
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
        private_launch.command.clone(),
    ]);
    Ok((args, private_launch))
}

struct PrivateLaunch {
    command: String,
    route_path: PathBuf,
}

impl Drop for PrivateLaunch {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.route_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return,
        }
        if let Some(parent) = self.route_path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

fn launch_command(request: &ProviderWrapperRequestV1) -> Result<String, &'static str> {
    let prompt = format!(
        "Resume tracked workstream {}. First run `shipyard --json work-ledger context-challenge --wake {}` and reconstruct that exact durable context. Write the matching receipt to a private file, then run `shipyard --json work-ledger acknowledge-context --wake {} --receipt <private-path>`. Complete the remaining work and keep Linear current. Before handoff, run `shipyard --json work-ledger return-challenge --ownership <ownership-id>`, write separate reviewed expectation and receipt files proving a newer checkpoint, evidence, and remote acknowledgement, then run `shipyard --json work-ledger return-ownership --ownership <ownership-id> --expectation <private-path> --receipt <private-path>`. Never put receipt JSON or secrets in argv.",
        request.resume_expectation.workstream_handle,
        request.delivery_fence.wake_id,
        request.delivery_fence.wake_id,
    );
    let mut lines = request
        .protected_route
        .environment
        .iter()
        .map(|(name, value)| {
            shell_word(&format!("{name}={value}")).map(|word| format!("export {word}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut invocation = request
        .protected_route
        .argv
        .iter()
        .map(|value| shell_word(value))
        .collect::<Result<Vec<_>, _>>()?;
    invocation.push(shell_word(&prompt)?);
    lines.push(format!("exec {}", invocation.join(" ")));
    Ok(lines.join("\n"))
}

fn prepare_private_launch(
    request: &ProviderWrapperRequestV1,
) -> Result<PrivateLaunch, &'static str> {
    let body = launch_command(request)?;
    let directory = tempfile::Builder::new()
        .prefix(".shipyard-workstream-route-")
        .tempdir()
        .map_err(|_| "private-launch-directory-unavailable")?;
    let directory_path = directory.path().to_path_buf();
    let route_path = directory_path.join("launch.sh");
    let mut route = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&route_path)
        .map_err(|_| "private-launch-file-unavailable")?;
    let directory_word = shell_word(
        directory_path
            .to_str()
            .ok_or("private-launch-path-invalid")?,
    )?;
    let prologue = format!(
        "#!/bin/sh\nset -eu\nroute_dir={directory_word}\nrm -f -- \"$0\"\nrmdir -- \"$route_dir\"\n"
    );
    route
        .write_all(prologue.as_bytes())
        .and_then(|()| route.write_all(body.as_bytes()))
        .and_then(|()| route.write_all(b"\n"))
        .and_then(|()| route.sync_all())
        .map_err(|_| "private-launch-file-unwritable")?;
    drop(route);
    sync_directory(&directory_path)?;
    let directory_path = directory.keep();
    let route_path = directory_path.join("launch.sh");
    Ok(PrivateLaunch {
        command: format!(
            "'/bin/sh' {}",
            shell_word(route_path.to_str().ok_or("private-launch-path-invalid")?)?
        ),
        route_path,
    })
}

fn sync_directory(path: &Path) -> Result<(), &'static str> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "private-launch-directory-unwritable")
}

fn shell_word(value: &str) -> Result<String, &'static str> {
    if value.is_empty()
        || value
            .chars()
            .any(|character| character == '\0' || character.is_control())
    {
        return Err("launch-value-is-not-shell-safe");
    }
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
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
    // cmux adds informational fields (currently `window_id` and `group_id`) to
    // this response. Acceptance depends only on the two required UUIDs, so
    // tolerate additive metadata while validating those identifiers strictly.
    Ok(CreatedWorkspaceIds {
        workspace_id: canonical_uuid(&created.workspace_id).ok_or(())?,
        surface_id: canonical_uuid(&created.surface_id).ok_or(())?,
    })
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
    let result = runner
        .run(&args)
        .map_err(|_| "cmux-surface-list-unavailable")?;
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
    let result = runner
        .run(&args)
        .map_err(|_| "cmux-session-evidence-unavailable")?;
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
        session_checkpoint_id: &'a str,
    }
    let receipt = serde_json::to_vec(&Receipt {
        domain: "shipyard-cmux-provider-receipt-v1",
        provider_id: &request.provider_id,
        idempotency_key: &request.delivery_fence.idempotency_key,
        workspace_id,
        description,
        session_checkpoint_id: &binding.checkpoint_id,
    })
    .expect("fixed receipt serialization cannot fail");
    ProviderWrapperOutcomeV1::Delivered {
        acceptance: ProviderAcceptanceV1::ProviderSessionAccepted,
        provider_session_ref: format!("session:{}:{}", request.provider_id, binding.checkpoint_id),
        receipt_digest: hex::encode(Sha256::digest(receipt)),
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
        format!("shipyard-cmux-provider-{class}-v1\0{code}").as_bytes(),
    ))
}

fn response(
    request: &ProviderWrapperRequestV1,
    outcome: ProviderWrapperOutcomeV1,
) -> ProviderWrapperResponseV1 {
    ProviderWrapperResponseV1 {
        schema_version: SCHEMA_VERSION,
        operation: request.operation,
        provider_id: request.provider_id.clone(),
        adapter_id: request.adapter_id.clone(),
        idempotency_key: request.delivery_fence.idempotency_key.clone(),
        outcome,
    }
}

#[cfg(test)]
mod tests;
