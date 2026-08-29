//! Digest-pinnable cmux adapter for one fresh workstream continuation session.
//!
//! The adapter owns no delivery or lifecycle state. It maps one strict wrapper
//! request to one strict response while using the delivery idempotency key as
//! cmux's durable lookup surface. Cargo registers the binary, but the current
//! release workflows and installer still package only `shipyard`; a production
//! follow-up must build, sign, install, and digest-pin this second executable.

#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::provider_wrapper::{
    NotAcceptedV1, PROVIDER_WRAPPER_SCHEMA_VERSION, ProviderAcceptanceV1, ProviderDeliveryTargetV1,
    ProviderWrapperOperationV1, ProviderWrapperOutcomeV1, ProviderWrapperRequestV1,
    ProviderWrapperResponseV1, TerminalEndpointV1, UnknownV1, validate_request,
};
use crate::workstream_continuation_config::{
    ProviderWrapperConfig, load_trusted_terminal_trust_config,
};

const ADAPTER_ID: &str = "cmux-workstream-v1";
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

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
        serde_json::from_slice(&bytes).map_err(|_| "request is not strict v2 JSON".to_owned())?;
    let terminal_trust = load_trusted_terminal_trust_config()
        .map_err(|_| "trusted terminal policy is unavailable".to_owned())?;
    let mut terminal = ProductionCmuxTransport::new(terminal_trust.cmux_signing_team_id);
    let mut provider = ProductionSubrouterLaunchAuthority;
    let response = handle_request(&request, &mut terminal, &mut provider);
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

mod provider_launch;
mod terminal_transport;

use provider_launch::{
    PRIVATE_LAUNCH_ACCEPTANCE_DEADLINE, PrivateLaunch, ProductionSubrouterLaunchAuthority,
    ProviderLaunchAuthority, delivery_prompt,
};
#[cfg(test)]
use provider_launch::{launch_command, prepare_private_launch, verify_subrouter_executable};
#[cfg(test)]
use terminal_transport::verify_cmux_signing_policy;
use terminal_transport::{ProductionCmuxTransport, RunnerFailure, TerminalTransport};

fn handle_request(
    request: &ProviderWrapperRequestV1,
    terminal: &mut impl TerminalTransport,
    provider: &mut impl ProviderLaunchAuthority,
) -> ProviderWrapperResponseV1 {
    if let Err(code) = validate_adapter_request(request) {
        let outcome = match request.operation {
            ProviderWrapperOperationV1::Submit => rejected(code),
            ProviderWrapperOperationV1::Reconcile => uncertain(code),
        };
        return response(request, outcome);
    }
    if let Some(outcome) = terminal_capability_refusal(request) {
        return response(request, outcome);
    }
    if let Some(outcome) = bind_terminal(request, terminal) {
        return response(request, outcome);
    }
    if let ProviderDeliveryTargetV1::OriginalSession { surface_id } = &request.delivery_target {
        return response(
            request,
            deliver_to_original_session(request, terminal, surface_id),
        );
    }
    let description = format!(
        "shipyard-workstream-delivery:{}",
        request.delivery_fence.idempotency_key
    );
    let listed = match list_matching_workspaces(terminal, &description) {
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
                reconcile_existing_workspace(request, terminal, workspace_id, &description),
            );
        }
        [] => {}
        _ => return response(request, uncertain("multiple-idempotency-workspaces")),
    }
    if request.operation == ProviderWrapperOperationV1::Reconcile
        || request.delivery_target == ProviderDeliveryTargetV1::ReconcileOnly
    {
        return response(request, uncertain("reconcile-visibility-not-yet-proven"));
    }
    if request.delivery_target != ProviderDeliveryTargetV1::FreshCheckpoint {
        return response(request, rejected("unsupported-delivery-target"));
    }

    // Terminal transport selection and provider launch authority are separate
    // trust decisions. Only the typed, validated protected route may cross
    // this boundary; a missing Subrouter is a refusal, never direct-provider
    // fallback through the selected terminal.
    // Exact executable bytes are launch authority, not observation authority.
    // Reconciliation above must remain able to prove an already accepted
    // session after the configured Subrouter binary has moved or upgraded.
    if let Err(code) = provider.verify_route(request) {
        return response(request, rejected(code));
    }

    let private_launch = match provider.prepare_launch(request) {
        Ok(launch) => launch,
        Err(code) => return response(request, rejected(code)),
    };
    let (args, private_launch) = create_args(request, &description, private_launch);
    // cmux creates the workspace before it sends `--command` to the surface.
    // From this invocation onward every failure is an ambiguous acceptance.
    let created_result = terminal.run(&args);
    if !private_launch.wait_until_consumed(PRIVATE_LAUNCH_ACCEPTANCE_DEADLINE) {
        return response(request, uncertain("cmux-private-launch-not-consumed"));
    }
    let created = match created_result {
        Ok(result) if result.success => result,
        Ok(_) | Err(_) => return response(request, uncertain("cmux-create-outcome-unknown")),
    };
    let Ok(created) = parse_created_workspace(&created.stdout) else {
        return response(request, uncertain("cmux-create-response-invalid"));
    };
    response(
        request,
        match session_binding_for_surface(
            terminal,
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

fn bind_terminal(
    request: &ProviderWrapperRequestV1,
    terminal: &mut impl TerminalTransport,
) -> Option<ProviderWrapperOutcomeV1> {
    match terminal.bind(&request.terminal_endpoint) {
        Err(RunnerFailure::CapabilityUnproven) => {
            let outcome = match request.operation {
                ProviderWrapperOperationV1::Submit => rejected("terminal-capability-unproven"),
                ProviderWrapperOperationV1::Reconcile => uncertain("terminal-capability-unproven"),
            };
            Some(outcome)
        }
        #[cfg(any(target_os = "macos", test))]
        Err(RunnerFailure::Untrusted) => {
            let outcome = match request.operation {
                ProviderWrapperOperationV1::Submit => rejected("cmux-untrusted"),
                ProviderWrapperOperationV1::Reconcile => uncertain("cmux-untrusted"),
            };
            Some(outcome)
        }
        Err(RunnerFailure::Unavailable) => {
            let outcome = match request.operation {
                ProviderWrapperOperationV1::Submit => retryable("cmux-unavailable-before-create"),
                ProviderWrapperOperationV1::Reconcile => {
                    uncertain("cmux-unavailable-during-reconcile")
                }
            };
            Some(outcome)
        }
        Ok(()) => None,
    }
}

fn terminal_capability_refusal(
    request: &ProviderWrapperRequestV1,
) -> Option<ProviderWrapperOutcomeV1> {
    matches!(request.terminal_endpoint, TerminalEndpointV1::HerdR { .. }).then(|| {
        match request.operation {
            ProviderWrapperOperationV1::Submit => rejected("herdr-capability-unproven"),
            ProviderWrapperOperationV1::Reconcile => uncertain("herdr-capability-unproven"),
        }
    })
}

fn reconcile_existing_workspace(
    request: &ProviderWrapperRequestV1,
    terminal: &mut impl TerminalTransport,
    workspace_id: &str,
    description: &str,
) -> ProviderWrapperOutcomeV1 {
    let bindings =
        match session_bindings_for_workspace(terminal, workspace_id, &request.provider_id) {
            Ok(bindings) => bindings,
            Err(code) => return uncertain(code),
        };
    match bindings.as_slice() {
        [binding] => delivered(request, workspace_id, description, binding),
        [] => uncertain("cmux-session-binding-not-yet-visible"),
        _ => uncertain("multiple-provider-session-bindings"),
    }
}

fn deliver_to_original_session(
    request: &ProviderWrapperRequestV1,
    terminal: &mut impl TerminalTransport,
    surface_id: &str,
) -> ProviderWrapperOutcomeV1 {
    let Some(surface_id) = canonical_uuid(surface_id) else {
        return rejected("original-surface-id-invalid");
    };
    let mut args = cmux_prefix(["surface", "resume", "show"]);
    args.extend(["--surface".to_owned(), surface_id.clone()]);
    let evidence = match terminal.run(&args) {
        Ok(result) if result.success => result,
        Ok(_) | Err(_) => return retryable("original-session-unavailable-before-send"),
    };
    let Ok(evidence) = serde_json::from_slice::<SurfaceResumeEvidence>(&evidence.stdout) else {
        return retryable("original-session-evidence-invalid");
    };
    let Some(mut binding) = evidence.resume_binding else {
        return retryable("original-session-binding-absent");
    };
    let Some(workspace_id) = canonical_uuid(&evidence.workspace_id) else {
        return retryable("original-workspace-id-invalid");
    };
    if canonical_uuid(&evidence.surface_id).as_deref() != Some(surface_id.as_str())
        || binding.kind != request.provider_id
        || binding.source != "agent-hook"
        || binding.checkpoint_id != request.protected_route.native_session_id
        || !binding.is_local()
    {
        return retryable("original-session-binding-changed");
    }
    binding.checkpoint_id = match canonical_uuid(&binding.checkpoint_id) {
        Some(checkpoint) => checkpoint,
        None => return retryable("original-session-checkpoint-invalid"),
    };
    let sent = terminal.run(&[
        "send".to_owned(),
        "--surface".to_owned(),
        surface_id.clone(),
        delivery_prompt(request),
    ]);
    match sent {
        Ok(result) if result.success => {}
        Ok(_) => return retryable("original-session-send-refused"),
        Err(_) => return uncertain("original-session-send-outcome-unknown"),
    }
    let entered = terminal.run(&[
        "send-key".to_owned(),
        "--surface".to_owned(),
        surface_id.clone(),
        "enter".to_owned(),
    ]);
    match entered {
        Ok(result) if result.success => delivered(
            request,
            &workspace_id,
            &format!("in-place:{surface_id}"),
            &binding,
        ),
        Ok(_) | Err(_) => uncertain("original-session-enter-outcome-unknown"),
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
    terminal: &mut impl TerminalTransport,
    description: &str,
) -> Result<Vec<String>, &'static str> {
    let windows_result = terminal
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
        let result = terminal
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
    private_launch: PrivateLaunch,
) -> (Vec<String>, PrivateLaunch) {
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
    (args, private_launch)
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
    execution_location: Option<String>,
    remote_pty_session_id: Option<String>,
    remote_surface_id: Option<String>,
    remote_workspace_id: Option<String>,
}

impl AgentSessionBinding {
    fn is_local(&self) -> bool {
        self.execution_location.as_deref() == Some("local")
            && self.remote_pty_session_id.is_none()
            && self.remote_surface_id.is_none()
            && self.remote_workspace_id.is_none()
    }
}

#[derive(Deserialize)]
struct SurfaceResumeEvidence {
    workspace_id: String,
    surface_id: String,
    resume_binding: Option<AgentSessionBinding>,
}

fn session_bindings_for_workspace(
    terminal: &mut impl TerminalTransport,
    workspace_id: &str,
    provider_id: &str,
) -> Result<Vec<AgentSessionBinding>, &'static str> {
    let mut args = cmux_prefix(["surface-health"]);
    args.extend(["--workspace".to_owned(), workspace_id.to_owned()]);
    let result = terminal
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
    for surface_id in &surface_ids {
        if let Some(binding) =
            session_binding_for_surface(terminal, workspace_id, surface_id, provider_id)?
        {
            bindings.push(binding);
        }
    }
    Ok(bindings)
}

fn session_binding_for_surface(
    terminal: &mut impl TerminalTransport,
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
    let result = terminal
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
    if binding.kind != provider_id || binding.source != "agent-hook" || !binding.is_local() {
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
        schema_version: PROVIDER_WRAPPER_SCHEMA_VERSION,
        operation: request.operation,
        provider_id: request.provider_id.clone(),
        adapter_id: request.adapter_id.clone(),
        idempotency_key: request.delivery_fence.idempotency_key.clone(),
        outcome,
    }
}

#[cfg(test)]
mod tests;
