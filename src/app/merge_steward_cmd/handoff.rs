use super::{
    CliFailure, GitHubActions, HANDOFF_CONTEXT, MANAGED_LABEL, Path, TerminalProvenanceKind,
    UNMANAGED_LABEL, Value, Write, gh_json, is_full_sha, observation::encode_path_segment,
    resolve_repos, write_json_envelope,
};
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::process::ExitCode;

use crate::paths::RuntimePaths;
use crate::queue::replace_file_with_windows_retry;
use crate::terminal_delivery_authority::{
    ProductionTerminalEvidenceAdapter, TerminalCapabilityRequest, TerminalEvidenceAdapter,
};

#[derive(Clone)]
pub(crate) struct StewardHandoffArgs {
    pub(crate) repo: Option<String>,
    pub(crate) pr: u64,
    pub(crate) head: String,
    pub(crate) workstream_id: String,
    pub(crate) context_url: Option<String>,
    pub(crate) agent_provider: Option<String>,
    pub(crate) agent_session_id: Option<String>,
    pub(crate) agent_parent_session_id: Option<String>,
    pub(crate) agent_surface_id: Option<String>,
    pub(crate) transfer_agent_owner: bool,
    pub(crate) apply: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HandoffPhase {
    Intent,
    Ready,
    Managed,
}

impl HandoffPhase {
    const fn rank(self) -> u8 {
        match self {
            Self::Intent => 0,
            Self::Ready => 1,
            Self::Managed => 2,
        }
    }

    const fn max(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct AgentResumeContext {
    provider: String,
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    surface_id: Option<String>,
    surface_provenance: SurfaceProvenance,
    #[serde(default)]
    terminal_provenance: TerminalProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_authority: Option<TerminalCapabilityRequest>,
    resume_transport: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct AgentRouteReference {
    route_id: String,
    owner_id: String,
    provider: String,
    origin_machine: String,
    resume_transport: String,
    #[serde(default)]
    terminal_provenance: TerminalProvenanceKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct StoredAgentRoute {
    schema_version: u32,
    route_id: String,
    owner_id: String,
    origin_machine: String,
    agent: AgentResumeContext,
    revision: u64,
    created_at: String,
    updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct StoredMachineIdentity {
    schema_version: u32,
    id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RepairRoute {
    OriginalAgent,
    FreshAgentOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SurfaceProvenance {
    Absent,
    Explicit,
    AmbientCmux,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TerminalProvenance {
    #[default]
    Absent,
    Cmux {
        surface_id: String,
    },
    HerdR {
        session_id: String,
        workspace_id: String,
        tab_id: String,
        pane_id: String,
        provider_session_id: String,
    },
}

impl TerminalProvenance {
    const fn kind(&self) -> TerminalProvenanceKind {
        match self {
            // Existing cmux surface provenance is advisory and may appear or
            // move without changing the immutable owner route. Keep its stored
            // route-reference contract compatible with pre-adapter receipts.
            Self::Absent | Self::Cmux { .. } => TerminalProvenanceKind::Absent,
            Self::HerdR { .. } => TerminalProvenanceKind::HerdR,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DurableStewardHandoff {
    schema_version: u32,
    repo: String,
    pr: u64,
    head_sha: String,
    workstream_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_url: Option<String>,
    origin_machine: String,
    owner_id: String,
    ownership_generation: u64,
    revision: u64,
    repair_route: RepairRoute,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_route: Option<AgentRouteReference>,
    phase: HandoffPhase,
    wake_consumer_available: bool,
    created_at: String,
    updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StewardHandoffTransferReport {
    pub(crate) wake_consumer_available: bool,
}

pub(crate) fn steward_handoff_command<W: Write>(
    args: &StewardHandoffArgs,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    // A plain `shipyard pr` handoff carries no explicit agent route, so it must
    // not be bound to one merely because the shell it ran in exports
    // CLAUDE_CODE_SESSION_ID or CODEX_THREAD_ID -- which every agent shell
    // does. Resolving it against a default environment keeps the
    // ambient fence for EXPLICIT routes, where it belongs, while letting the
    // legacy fallback through.
    if is_legacy_pr_fallback(args) {
        return steward_handoff_command_with_resolver(
            args,
            cwd,
            runtime_paths,
            actions,
            json_output,
            stdout,
            |args| resolve_agent_context_with_environment(args, &AgentEnvironment::default()),
        );
    }
    steward_handoff_command_with_resolver(
        args,
        cwd,
        runtime_paths,
        actions,
        json_output,
        stdout,
        resolve_agent_context,
    )
}

#[cfg(test)]
pub(crate) fn steward_handoff_command_without_ambient<W: Write>(
    args: &StewardHandoffArgs,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    steward_handoff_command_with_resolver(
        args,
        cwd,
        runtime_paths,
        actions,
        json_output,
        stdout,
        |args| resolve_agent_context_with_environment(args, &AgentEnvironment::default()),
    )
}

#[allow(clippy::too_many_lines)]
fn steward_handoff_command_with_resolver<W: Write, F>(
    args: &StewardHandoffArgs,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
    resolve_agent: F,
) -> Result<ExitCode, CliFailure>
where
    F: FnOnce(&StewardHandoffArgs) -> Result<Option<AgentResumeContext>, CliFailure>,
{
    validate_args(args)?;
    let repo = resolve_repos(args.repo.clone().into_iter().collect(), cwd)?
        .into_iter()
        .next()
        .ok_or_else(|| CliFailure::new(1, "repository was not resolved"))?;
    verify_exact_open_pr(actions, &repo, args.pr, &args.head)?;
    let agent = resolve_handoff_agent(args, resolve_agent)?;
    let origin_machine = if args.apply {
        resolve_origin_machine(runtime_paths)?
    } else {
        preview_origin_machine(runtime_paths)?
    };
    let agent_route = agent
        .as_ref()
        .map(|agent| agent_route_reference(agent, &origin_machine));

    let mut wake_consumer_available = false;
    if args.apply {
        let directory = handoff_directory(runtime_paths, &repo, args.pr);
        ensure_private_directory(&directory)?;
        let _handoff_lock = acquire_handoff_lock(&directory, &args.head)?;
        let path = handoff_path(&directory, &args.head);
        let route_path = agent_route
            .as_ref()
            .map(|route| agent_route_path(runtime_paths, &route.route_id));
        let mut receipt = prepare_handoff_receipt(
            load_handoff(&path)?,
            args,
            &repo,
            &origin_machine,
            agent_route.clone(),
        )?;
        let starting_phase = receipt.phase;
        if let (Some(agent), Some(route), Some(route_path)) =
            (agent.as_ref(), agent_route.as_ref(), route_path.as_ref())
        {
            persist_agent_route_with_transfer(route_path, route, agent, args.transfer_agent_owner)?;
        }
        // The local intent is durable before the first GitHub mutation. A
        // restarted same-owner invocation can safely replay the idempotent
        // remote writes and advance this exact record without adopting a new
        // owner or head.
        receipt = persist_handoff(&path, receipt, HandoffPhase::Intent)?;
        if !handoff_status_is_present(actions, &repo, args)? {
            write_handoff_status(actions, &repo, args)?;
        }
        // A status written to a superseded commit is harmless. The management
        // label is not: re-read immediately before adding it so a newer head
        // cannot be adopted using the old receipt.
        verify_exact_open_pr(actions, &repo, args.pr, &args.head)?;
        if starting_phase == HandoffPhase::Intent {
            receipt = persist_handoff(&path, receipt, HandoffPhase::Ready)?;
        }
        ensure_label(
            actions,
            &repo,
            MANAGED_LABEL,
            "0E8A16",
            "Explicit Shipyard stewardship ownership",
        )?;
        add_label(actions, &repo, args.pr, MANAGED_LABEL)?;
        verify_exact_open_pr(actions, &repo, args.pr, &args.head)?;
        receipt = persist_handoff(&path, receipt, HandoffPhase::Managed)?;
        wake_consumer_available = receipt.wake_consumer_available;
        remove_label(actions, &repo, args.pr, UNMANAGED_LABEL)?;
        debug_assert_eq!(receipt.phase, HandoffPhase::Managed);
    }

    render(
        args,
        &repo,
        agent_route.as_ref(),
        &origin_machine,
        json_output,
        wake_consumer_available,
        stdout,
    )?;
    Ok(ExitCode::SUCCESS)
}

fn validate_args(args: &StewardHandoffArgs) -> Result<(), CliFailure> {
    if args.pr == 0 {
        return Err(CliFailure::new(1, "pull-request number must be positive"));
    }
    if !is_full_sha(&args.head) {
        return Err(CliFailure::new(
            1,
            "--head must be a full 40-character SHA-1",
        ));
    }
    if let Some(url) = args.context_url.as_deref()
        && !(url.starts_with("https://") || url.starts_with("http://"))
    {
        return Err(CliFailure::new(
            1,
            "--context-url must use http:// or https://",
        ));
    }
    if args.transfer_agent_owner
        && (args.agent_provider.is_none() || args.agent_session_id.is_none())
    {
        return Err(CliFailure::new(
            1,
            "--transfer-agent-owner requires explicit --agent-provider and --agent-session-id",
        ));
    }
    Ok(())
}

fn is_legacy_pr_fallback(args: &StewardHandoffArgs) -> bool {
    args.agent_provider.is_none()
        && args.agent_session_id.is_none()
        && args.agent_parent_session_id.is_none()
        && args.agent_surface_id.is_none()
        && !args.transfer_agent_owner
        && args.repo.as_deref().is_some_and(|repository| {
            // Canonicalise the SLUG, but keep the id comparison exact.
            // Requiring the slug to be already-lowercase made this hatch
            // unreachable for any repo whose owner carries a capital, which is
            // most of them. Comparing the id case-insensitively would be too
            // loose in the other direction -- `OWNER/repo#7` must still be
            // rejected for `owner/repo`.
            let normalized = repository.to_ascii_lowercase();
            normalized.split('/').count() == 2
                && args.workstream_id == format!("{normalized}#{}", args.pr)
        })
}

fn validate_resolved_workstream_identity(
    args: &StewardHandoffArgs,
    agent: Option<&AgentResumeContext>,
) -> Result<(), CliFailure> {
    if is_legacy_pr_fallback(args) && agent.is_some() {
        return Err(CliFailure::new(
            1,
            "legacy PR fallback cannot bind an agent route or managed lifecycle",
        ));
    }
    Ok(())
}

fn resolve_handoff_agent<F>(
    args: &StewardHandoffArgs,
    resolve_agent: F,
) -> Result<Option<AgentResumeContext>, CliFailure>
where
    F: FnOnce(&StewardHandoffArgs) -> Result<Option<AgentResumeContext>, CliFailure>,
{
    let agent = resolve_agent(args)?;
    validate_resolved_workstream_identity(args, agent.as_ref())?;
    Ok(agent)
}

#[derive(Clone, Default)]
struct AgentEnvironment {
    codex_session: Option<String>,
    claude_session: Option<String>,
    surface_id: Option<String>,
    herdr_env: Option<String>,
    herdr_session: Option<String>,
    herdr_workspace_id: Option<String>,
    herdr_tab_id: Option<String>,
    herdr_pane_id: Option<String>,
}

fn resolve_agent_context(
    args: &StewardHandoffArgs,
) -> Result<Option<AgentResumeContext>, CliFailure> {
    let environment = AgentEnvironment {
        codex_session: env::var("CODEX_THREAD_ID")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        claude_session: env::var("CLAUDE_CODE_SESSION_ID")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        surface_id: env::var("CMUX_SURFACE_ID")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        herdr_env: env::var("HERDR_ENV").ok(),
        herdr_session: env::var("HERDR_SESSION").ok(),
        herdr_workspace_id: env::var("HERDR_WORKSPACE_ID").ok(),
        herdr_tab_id: env::var("HERDR_TAB_ID").ok(),
        herdr_pane_id: env::var("HERDR_PANE_ID").ok(),
    };
    let mut resolved = resolve_agent_context_with_environment(args, &environment)?;
    if let Some(agent) = resolved.as_mut() {
        agent.terminal_authority = match &agent.terminal_provenance {
            TerminalProvenance::Cmux { surface_id } => {
                let socket_path = env::var("CMUX_SOCKET_PATH")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        CliFailure::new(1, "cmux terminal authority requires CMUX_SOCKET_PATH")
                    })?;
                let cli_path = resolve_path_executable("cmux")?;
                Some(
                    ProductionTerminalEvidenceAdapter
                        .capture_cmux(
                            &cli_path,
                            &socket_path,
                            surface_id,
                            &agent.session_id,
                            &agent.provider,
                        )
                        .map_err(|failure| {
                            CliFailure::new(
                                1,
                                format!("cmux terminal authority refused: {failure:?}"),
                            )
                        })?,
                )
            }
            TerminalProvenance::HerdR {
                session_id,
                pane_id,
                ..
            } => Some(TerminalCapabilityRequest::HerdR {
                selector: session_id.clone(),
                terminal_id: Some(pane_id.clone()),
                native_session_id: agent.session_id.clone(),
                provider_kind: agent.provider.clone(),
            }),
            TerminalProvenance::Absent => None,
        };
    }
    Ok(resolved)
}

fn resolve_path_executable(name: &str) -> Result<String, CliFailure> {
    let path = env::var_os("PATH")
        .into_iter()
        .flat_map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| CliFailure::new(1, format!("{name} executable is unavailable")))?;
    path.canonicalize()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| CliFailure::new(1, format!("resolve {name} executable: {error}")))
}

fn resolve_agent_context_with_environment(
    args: &StewardHandoffArgs,
    environment: &AgentEnvironment,
) -> Result<Option<AgentResumeContext>, CliFailure> {
    let explicit_provider = args.agent_provider.as_deref();
    let explicit_session = args.agent_session_id.as_deref();
    if explicit_provider.is_some() != explicit_session.is_some() {
        return Err(CliFailure::new(
            1,
            "--agent-provider and --agent-session-id must be supplied together",
        ));
    }
    let codex_session = environment.codex_session.clone();
    let claude_session = environment.claude_session.clone();
    if explicit_provider.is_none() && codex_session.is_some() && claude_session.is_some() {
        return Err(CliFailure::new(
            1,
            "both Codex and Claude session environments are present; pass --agent-provider and --agent-session-id explicitly",
        ));
    }
    let captured = match (explicit_provider, explicit_session) {
        (Some(provider), Some(session_id)) => Some((provider.to_owned(), session_id.to_owned())),
        (None, None) => codex_session
            .map(|session_id| ("codex".to_owned(), session_id))
            .or_else(|| claude_session.map(|session_id| ("claude".to_owned(), session_id))),
        _ => unreachable!("provider/session parity checked above"),
    };
    let Some((provider, session_id)) = captured else {
        if herdr_route_input_present(environment) {
            return Err(CliFailure::new(
                1,
                "HerdR terminal route input requires a resumable agent session",
            ));
        }
        if args.agent_parent_session_id.is_some() || args.agent_surface_id.is_some() {
            return Err(CliFailure::new(
                1,
                "agent parent/surface route fields require a resumable agent session",
            ));
        }
        return Ok(None);
    };
    validate_agent_identifier("agent session", &session_id)?;
    if let Some(parent) = args.agent_parent_session_id.as_deref() {
        validate_agent_identifier("parent agent session", parent)?;
    }
    let (surface_id, surface_provenance) = if let Some(surface) = args.agent_surface_id.as_deref() {
        (Some(surface.to_owned()), SurfaceProvenance::Explicit)
    } else if let Some(surface) = environment
        .surface_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        (Some(surface.to_owned()), SurfaceProvenance::AmbientCmux)
    } else {
        (None, SurfaceProvenance::Absent)
    };
    if let Some(surface) = surface_id.as_deref() {
        validate_agent_identifier("agent surface", surface)?;
    }
    let terminal_provenance =
        resolve_terminal_provenance(environment, &session_id, surface_id.as_deref())?;
    Ok(Some(AgentResumeContext {
        resume_transport: match provider.as_str() {
            "codex" => "codex_queue".to_owned(),
            "claude" => "claude_resume".to_owned(),
            _ => {
                return Err(CliFailure::new(1, "agent provider must be codex or claude"));
            }
        },
        provider,
        session_id,
        parent_session_id: args.agent_parent_session_id.clone(),
        surface_id,
        surface_provenance,
        terminal_provenance,
        terminal_authority: None,
    }))
}

fn herdr_route_input_present(environment: &AgentEnvironment) -> bool {
    environment.herdr_env.is_some()
        || environment.herdr_session.is_some()
        || environment.herdr_workspace_id.is_some()
        || environment.herdr_tab_id.is_some()
        || environment.herdr_pane_id.is_some()
}

fn resolve_terminal_provenance(
    environment: &AgentEnvironment,
    provider_session_id: &str,
    surface_id: Option<&str>,
) -> Result<TerminalProvenance, CliFailure> {
    let herdr_route_fields = [
        environment.herdr_workspace_id.as_deref(),
        environment.herdr_tab_id.as_deref(),
        environment.herdr_pane_id.as_deref(),
    ];
    match environment.herdr_env.as_deref() {
        Some("1") => {
            if surface_id.is_some() {
                return Err(CliFailure::new(
                    1,
                    "HerdR and cmux terminal routes cannot be combined",
                ));
            }
            let [Some(workspace_id), Some(tab_id), Some(pane_id)] = herdr_route_fields else {
                return Err(CliFailure::new(
                    1,
                    "HERDR_ENV=1 requires workspace, tab, and pane identifiers; HERDR_SESSION is optional and defaults to default",
                ));
            };
            let session_id = environment.herdr_session.as_deref().unwrap_or("default");
            for (label, value) in [
                ("HerdR session", session_id),
                ("HerdR workspace", workspace_id),
                ("HerdR tab", tab_id),
                ("HerdR pane", pane_id),
                ("agent session", provider_session_id),
            ] {
                validate_agent_identifier(label, value)?;
            }
            // HerdR 0.8.2 exports terminal routing identity, but not the provider
            // session. Preserve the already-resolved Shipyard agent provenance as
            // the sole provider-session authority instead of trusting an invented
            // ambient variable.
            Ok(TerminalProvenance::HerdR {
                session_id: session_id.to_owned(),
                workspace_id: workspace_id.to_owned(),
                tab_id: tab_id.to_owned(),
                pane_id: pane_id.to_owned(),
                provider_session_id: provider_session_id.to_owned(),
            })
        }
        Some(_) => Err(CliFailure::new(
            1,
            "HERDR_ENV must be exactly 1 when present",
        )),
        None if environment.herdr_session.is_some()
            || herdr_route_fields.iter().any(Option::is_some) =>
        {
            Err(CliFailure::new(
                1,
                "HerdR route fields require explicit HERDR_ENV=1",
            ))
        }
        None => Ok(surface_id.map_or(TerminalProvenance::Absent, |surface| {
            TerminalProvenance::Cmux {
                surface_id: surface.to_owned(),
            }
        })),
    }
}

fn validate_agent_identifier(label: &str, value: &str) -> Result<(), CliFailure> {
    let valid = !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'));
    if valid {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!("{label} must be 1-256 safe identifier characters"),
        ))
    }
}

fn resolve_origin_machine(runtime_paths: &RuntimePaths) -> Result<String, CliFailure> {
    ensure_private_directory(&runtime_paths.state_dir)?;
    let lock_path = runtime_paths.state_dir.join("machine-identity.lock");
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&lock_path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| CliFailure::new(1, format!("open machine identity lock: {error}")))?;
    lock.lock_exclusive()
        .map_err(|error| CliFailure::new(1, format!("lock machine identity: {error}")))?;

    let identity_path = runtime_paths.state_dir.join("machine-identity.json");
    match fs::read(&identity_path) {
        Ok(bytes) => {
            let identity: StoredMachineIdentity =
                serde_json::from_slice(&bytes).map_err(|error| {
                    CliFailure::new(1, format!("invalid stored machine identity: {error}"))
                })?;
            if identity.schema_version != 1 {
                return Err(CliFailure::new(
                    1,
                    "unsupported stored machine identity schema",
                ));
            }
            validate_agent_identifier("origin machine", &identity.id)?;
            return Ok(identity.id);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("read stored machine identity: {error}"),
            ));
        }
    }

    let tag_path = runtime_paths.state_dir.join("machine-tag");
    let id = match fs::read_to_string(&tag_path) {
        Ok(raw) => {
            let tag = raw.trim();
            crate::runner_provision::validate_machine_tag(tag).map_err(|error| {
                CliFailure::new(1, format!("invalid stored machine tag: {error}"))
            })?;
            tag.to_owned()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => opaque_id(
            "machine",
            &[
                &Utc::now().to_rfc3339(),
                &std::process::id().to_string(),
                &runtime_paths.state_dir.to_string_lossy(),
            ],
        ),
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("read stored machine tag: {error}"),
            ));
        }
    };
    validate_agent_identifier("origin machine", &id)?;
    save_private_json(
        &identity_path,
        &StoredMachineIdentity {
            schema_version: 1,
            id: id.clone(),
        },
        "machine identity",
    )?;
    Ok(id)
}

fn preview_origin_machine(runtime_paths: &RuntimePaths) -> Result<String, CliFailure> {
    let identity_path = runtime_paths.state_dir.join("machine-identity.json");
    match fs::read(&identity_path) {
        Ok(bytes) => {
            let identity: StoredMachineIdentity =
                serde_json::from_slice(&bytes).map_err(|error| {
                    CliFailure::new(1, format!("invalid stored machine identity: {error}"))
                })?;
            if identity.schema_version != 1 {
                return Err(CliFailure::new(
                    1,
                    "unsupported stored machine identity schema",
                ));
            }
            validate_agent_identifier("origin machine", &identity.id)?;
            return Ok(identity.id);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("read stored machine identity: {error}"),
            ));
        }
    }
    match fs::read_to_string(runtime_paths.state_dir.join("machine-tag")) {
        Ok(raw) => {
            let tag = raw.trim();
            crate::runner_provision::validate_machine_tag(tag).map_err(|error| {
                CliFailure::new(1, format!("invalid stored machine tag: {error}"))
            })?;
            Ok(tag.to_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok("unpersisted-machine".to_owned())
        }
        Err(error) => Err(CliFailure::new(
            1,
            format!("read stored machine tag: {error}"),
        )),
    }
}

fn opaque_id(prefix: &str, fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prefix.as_bytes());
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    format!("{prefix}-{}", hex::encode(hasher.finalize()))
}

fn agent_route_reference(agent: &AgentResumeContext, origin_machine: &str) -> AgentRouteReference {
    let owner_id = opaque_id("owner", &[&agent.provider, &agent.session_id]);
    let route_id = match &agent.terminal_provenance {
        TerminalProvenance::HerdR {
            session_id,
            workspace_id,
            tab_id,
            pane_id,
            provider_session_id,
        } => {
            let terminal_binding = opaque_id(
                "herdr",
                &[
                    session_id,
                    workspace_id,
                    tab_id,
                    pane_id,
                    provider_session_id,
                ],
            );
            opaque_id(
                "route",
                &[
                    origin_machine,
                    &agent.provider,
                    &agent.session_id,
                    agent.parent_session_id.as_deref().unwrap_or_default(),
                    // Preserved verbatim so existing durable route IDs keep
                    // hashing to the same value.
                    "session",
                    &agent.resume_transport,
                    &terminal_binding,
                ],
            )
        }
        TerminalProvenance::Absent | TerminalProvenance::Cmux { .. } => opaque_id(
            "route",
            &[
                origin_machine,
                &agent.provider,
                &agent.session_id,
                agent.parent_session_id.as_deref().unwrap_or_default(),
                // Preserved verbatim so existing durable route IDs keep
                // hashing to the same value.
                "session",
                &agent.resume_transport,
            ],
        ),
    };
    AgentRouteReference {
        route_id,
        owner_id,
        provider: agent.provider.clone(),
        origin_machine: origin_machine.to_owned(),
        resume_transport: agent.resume_transport.clone(),
        terminal_provenance: agent.terminal_provenance.kind(),
    }
}

fn handoff_directory(runtime_paths: &RuntimePaths, repo: &str, pr: u64) -> std::path::PathBuf {
    runtime_paths
        .state_dir
        .join("merge-steward")
        .join("handoffs")
        .join(encode_path_segment(&repo.to_ascii_lowercase()))
        .join(format!("pr-{pr}"))
}

fn handoff_path(directory: &Path, head: &str) -> std::path::PathBuf {
    directory.join(format!("{}.json", head.to_ascii_lowercase()))
}
pub(crate) fn steward_handoff_transfer_report(
    runtime_paths: &RuntimePaths,
    repo: &str,
    pr: u64,
    head: &str,
) -> Result<StewardHandoffTransferReport, CliFailure> {
    let path = handoff_path(&handoff_directory(runtime_paths, repo, pr), head);
    let receipt = load_handoff(&path)?
        .ok_or_else(|| CliFailure::new(1, "exact-head durable handoff receipt is unavailable"))?;
    validate_handoff_receipt_integrity(&receipt, repo, pr, head)?;
    if receipt.phase != HandoffPhase::Managed {
        return Err(CliFailure::new(1, "durable handoff is not managed"));
    }
    Ok(StewardHandoffTransferReport {
        wake_consumer_available: receipt.wake_consumer_available,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TerminalOwnerRoute {
    pub(super) origin_machine: String,
    pub(super) owner_id: String,
    pub(super) ownership_generation: u64,
    pub(super) owner_disposition: String,
    pub(super) route_id: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) resume_transport: Option<String>,
    pub(super) terminal_provenance: Option<TerminalProvenanceKind>,
}

pub(super) fn terminal_owner_route(
    state_dir: &Path,
    repo: &str,
    pr: u64,
    head: &str,
) -> Result<Option<TerminalOwnerRoute>, CliFailure> {
    let path = state_dir
        .join("merge-steward")
        .join("handoffs")
        .join(encode_path_segment(&repo.to_ascii_lowercase()))
        .join(format!("pr-{pr}"))
        .join(format!("{}.json", head.to_ascii_lowercase()));
    let Some(receipt) = load_handoff(&path)? else {
        return Ok(None);
    };
    validate_handoff_receipt_integrity(&receipt, repo, pr, head)?;
    if receipt.phase != HandoffPhase::Managed {
        return Ok(None);
    }
    let route = receipt.agent_route;
    let (owner_id, terminal_provenance) = if let Some(route) = route.as_ref() {
        let stored_path = state_dir
            .join("merge-steward")
            .join("agent-routes")
            .join(format!("{}.json", route.route_id));
        let stored = load_agent_route(&stored_path)?
            .ok_or_else(|| CliFailure::new(1, "managed handoff lost its private agent route"))?;
        let recomputed = agent_route_reference(&stored.agent, &stored.origin_machine);
        if stored.schema_version != 2
            || stored.revision == 0
            || stored.route_id != route.route_id
            || stored.owner_id != route.owner_id
            || stored.origin_machine != receipt.origin_machine
            || recomputed != *route
        {
            return Err(CliFailure::new(
                1,
                "managed handoff and private agent route identity disagree",
            ));
        }
        (
            opaque_id(
                "owner",
                &[
                    &stored.agent.provider,
                    stored
                        .agent
                        .parent_session_id
                        .as_deref()
                        .unwrap_or(&stored.agent.session_id),
                ],
            ),
            Some(match stored.agent.terminal_provenance {
                TerminalProvenance::Absent => TerminalProvenanceKind::Absent,
                TerminalProvenance::Cmux { .. } => TerminalProvenanceKind::Cmux,
                TerminalProvenance::HerdR { .. } => TerminalProvenanceKind::HerdR,
            }),
        )
    } else {
        (receipt.owner_id.clone(), None)
    };
    Ok(Some(TerminalOwnerRoute {
        origin_machine: receipt.origin_machine,
        owner_id,
        ownership_generation: receipt.ownership_generation,
        owner_disposition: if route.is_some() {
            "original_owner"
        } else {
            "fresh_agent_only"
        }
        .to_owned(),
        route_id: route.as_ref().map(|route| route.route_id.clone()),
        provider: route.as_ref().map(|route| route.provider.clone()),
        terminal_provenance,
        resume_transport: route.map(|route| route.resume_transport),
    }))
}

pub(super) fn terminal_owner_route_or_unresolved(
    state_dir: &Path,
    repo: &str,
    pr: u64,
    head: &str,
) -> Option<TerminalOwnerRoute> {
    // Route transport is not deployed authority. Corrupt, missing, or stale
    // private state therefore remains an unroutable obligation instead of
    // blocking deterministic stewardship or authorizing a fresh agent.
    match terminal_owner_route(state_dir, repo, pr, head) {
        Ok(owner) => owner,
        Err(_) => unresolved_terminal_owner(state_dir, repo, pr, head),
    }
}

fn unresolved_terminal_owner(
    state_dir: &Path,
    repo: &str,
    pr: u64,
    head: &str,
) -> Option<TerminalOwnerRoute> {
    let path = state_dir
        .join("merge-steward")
        .join("handoffs")
        .join(encode_path_segment(&repo.to_ascii_lowercase()))
        .join(format!("pr-{pr}"))
        .join(format!("{}.json", head.to_ascii_lowercase()));
    let receipt = load_handoff(&path).ok().flatten()?;
    if validate_handoff_receipt_integrity(&receipt, repo, pr, head).is_err()
        || receipt.phase != HandoffPhase::Managed
    {
        return None;
    }
    Some(TerminalOwnerRoute {
        origin_machine: receipt.origin_machine,
        owner_id: receipt.owner_id,
        ownership_generation: receipt.ownership_generation,
        owner_disposition: "unroutable_private_route".to_owned(),
        route_id: None,
        provider: None,
        resume_transport: None,
        terminal_provenance: None,
    })
}

fn agent_route_path(runtime_paths: &RuntimePaths, route_id: &str) -> std::path::PathBuf {
    runtime_paths
        .state_dir
        .join("merge-steward")
        .join("agent-routes")
        .join(format!("{route_id}.json"))
}

fn ensure_private_directory(directory: &Path) -> Result<(), CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(directory)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    crate::writer_domain_lease::ensure_protected_dir_all(directory)
        .map_err(|error| CliFailure::new(1, format!("create handoff directory: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|error| {
            CliFailure::new(
                1,
                format!("protect handoff directory {}: {error}", directory.display()),
            )
        })?;
    }
    Ok(())
}

fn acquire_handoff_lock(directory: &Path, head: &str) -> Result<fs::File, CliFailure> {
    let lock_path = directory.join(format!("{}.lock", head.to_ascii_lowercase()));
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&lock_path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| CliFailure::new(1, format!("open handoff lock: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))
            .map_err(|error| CliFailure::new(1, format!("protect handoff lock: {error}")))?;
    }
    file.try_lock_exclusive().map_err(|error| {
        CliFailure::new(
            1,
            format!("another handoff transition owns this exact PR head: {error}"),
        )
    })?;
    Ok(file)
}

fn load_handoff(path: &Path) -> Result<Option<DurableStewardHandoff>, CliFailure> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            CliFailure::new(1, format!("invalid durable handoff receipt: {error}"))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CliFailure::new(
            1,
            format!("read durable handoff receipt: {error}"),
        )),
    }
}

#[allow(clippy::too_many_lines)]
fn prepare_handoff_receipt(
    existing: Option<DurableStewardHandoff>,
    args: &StewardHandoffArgs,
    repo: &str,
    origin_machine: &str,
    agent_route: Option<AgentRouteReference>,
) -> Result<DurableStewardHandoff, CliFailure> {
    let normalized_repo = repo.to_ascii_lowercase();
    let normalized_head = args.head.to_ascii_lowercase();
    let owner_id = agent_route.as_ref().map_or_else(
        || "fresh-agent-only".to_owned(),
        |route| route.owner_id.clone(),
    );
    if let Some(existing) = existing {
        validate_existing_handoff(&existing, args, &normalized_repo, &normalized_head)?;
        if args.transfer_agent_owner {
            return transfer_handoff_owner(existing, args, origin_machine, owner_id, agent_route);
        }
        if existing.owner_id != owner_id {
            return Err(CliFailure::new(
                1,
                "this exact PR head already belongs to a different agent owner; explicit ownership transfer is required",
            ));
        }
        if existing.agent_route != agent_route {
            return Err(CliFailure::new(
                1,
                "same-owner handoff route metadata changed; explicit ownership transfer is required",
            ));
        }
        if existing.workstream_id != args.workstream_id || existing.context_url != args.context_url
        {
            return Err(CliFailure::new(
                1,
                "same-owner handoff cannot change workstream identity or context URL",
            ));
        }
        if existing.origin_machine != origin_machine {
            return Err(CliFailure::new(
                1,
                "same-owner handoff origin machine changed; explicit ownership transfer is required",
            ));
        }
        return Ok(existing);
    }
    if args.transfer_agent_owner {
        return Err(CliFailure::new(
            1,
            "--transfer-agent-owner requires an existing exact-head handoff receipt",
        ));
    }
    Ok(new_handoff_receipt(
        args,
        normalized_repo,
        normalized_head,
        origin_machine,
        owner_id,
        agent_route,
    ))
}

#[allow(clippy::too_many_arguments)]
fn new_handoff_receipt(
    args: &StewardHandoffArgs,
    normalized_repo: String,
    normalized_head: String,
    origin_machine: &str,
    owner_id: String,
    agent_route: Option<AgentRouteReference>,
) -> DurableStewardHandoff {
    let now = Utc::now().to_rfc3339();
    DurableStewardHandoff {
        schema_version: 4,
        repo: normalized_repo,
        pr: args.pr,
        head_sha: normalized_head,
        workstream_id: args.workstream_id.clone(),
        context_url: args.context_url.clone(),
        origin_machine: origin_machine.to_owned(),
        owner_id,
        ownership_generation: 1,
        revision: 0,
        repair_route: if agent_route.is_some() {
            RepairRoute::OriginalAgent
        } else {
            RepairRoute::FreshAgentOnly
        },
        agent_route,
        wake_consumer_available: false,
        phase: HandoffPhase::Intent,
        created_at: now.clone(),
        updated_at: now,
    }
}

fn validate_existing_handoff(
    existing: &DurableStewardHandoff,
    args: &StewardHandoffArgs,
    normalized_repo: &str,
    normalized_head: &str,
) -> Result<(), CliFailure> {
    validate_handoff_receipt_integrity(existing, normalized_repo, args.pr, normalized_head)
}

#[allow(clippy::too_many_lines)]
fn validate_handoff_receipt_integrity(
    receipt: &DurableStewardHandoff,
    repo: &str,
    pr: u64,
    head: &str,
) -> Result<(), CliFailure> {
    let route_consistent = receipt.agent_route.as_ref().map_or_else(
        || {
            receipt.repair_route == RepairRoute::FreshAgentOnly
                && receipt.owner_id == "fresh-agent-only"
        },
        |route| {
            receipt.repair_route == RepairRoute::OriginalAgent
                && receipt.owner_id == route.owner_id
                && receipt.origin_machine == route.origin_machine
        },
    );
    if !matches!(receipt.schema_version, 2..=4)
        || !receipt.repo.eq_ignore_ascii_case(repo)
        || receipt.pr != pr
        || !receipt.head_sha.eq_ignore_ascii_case(head)
        || receipt.ownership_generation == 0
        || receipt.revision == 0
        || !route_consistent
        || receipt.wake_consumer_available
    {
        return Err(CliFailure::new(
            1,
            "durable handoff receipt is incompatible or does not match its exact-head path",
        ));
    }
    validate_agent_identifier("origin machine", &receipt.origin_machine)?;
    Ok(())
}

fn transfer_handoff_owner(
    mut existing: DurableStewardHandoff,
    args: &StewardHandoffArgs,
    origin_machine: &str,
    owner_id: String,
    agent_route: Option<AgentRouteReference>,
) -> Result<DurableStewardHandoff, CliFailure> {
    if agent_route.is_none() {
        return Err(CliFailure::new(
            1,
            "--transfer-agent-owner requires an explicit replacement agent route",
        ));
    }
    if existing.workstream_id != args.workstream_id || existing.context_url != args.context_url {
        return Err(CliFailure::new(
            1,
            "ownership transfer cannot change workstream or context",
        ));
    }
    if existing.owner_id == owner_id && existing.agent_route == agent_route {
        return Ok(existing);
    }
    existing.owner_id = owner_id;
    existing.agent_route = agent_route;
    origin_machine.clone_into(&mut existing.origin_machine);
    existing.repair_route = RepairRoute::OriginalAgent;
    let next_generation = existing
        .ownership_generation
        .checked_add(1)
        .ok_or_else(|| CliFailure::new(1, "handoff ownership generation overflow"))?;
    existing.ownership_generation = next_generation;
    Ok(existing)
}

fn persist_handoff(
    path: &Path,
    mut receipt: DurableStewardHandoff,
    requested_phase: HandoffPhase,
) -> Result<DurableStewardHandoff, CliFailure> {
    receipt.phase = receipt.phase.max(requested_phase);
    receipt.revision = receipt
        .revision
        .checked_add(1)
        .ok_or_else(|| CliFailure::new(1, "handoff receipt revision overflow"))?;
    receipt.updated_at = Utc::now().to_rfc3339();
    save_private_json(path, &receipt, "handoff receipt")?;
    Ok(receipt)
}

#[cfg(test)]
fn persist_agent_route(
    path: &Path,
    route: &AgentRouteReference,
    agent: &AgentResumeContext,
) -> Result<(), CliFailure> {
    persist_agent_route_with_transfer(path, route, agent, false)
}

fn persist_agent_route_with_transfer(
    path: &Path,
    route: &AgentRouteReference,
    agent: &AgentResumeContext,
    allow_explicit_surface_change: bool,
) -> Result<(), CliFailure> {
    let _route_lock = acquire_agent_route_lock(path)?;
    if agent_route_reference(agent, &route.origin_machine) != *route {
        return Err(CliFailure::new(
            1,
            "agent route reference does not match its provider/session/origin contract",
        ));
    }
    if let Some(mut existing) = load_agent_route(path)? {
        if existing.schema_version != 2
            || existing.revision == 0
            || existing.route_id != route.route_id
            || existing.owner_id != route.owner_id
            || existing.origin_machine != route.origin_machine
            || !same_immutable_agent_contract(&existing.agent, agent)
        {
            return Err(CliFailure::new(1, "opaque agent-route identity collision"));
        }
        let reconciled =
            reconcile_surface_route(&existing.agent, agent, allow_explicit_surface_change)?;
        if reconciled == existing.agent {
            return Ok(());
        }
        existing.agent = reconciled;
        existing.revision = existing
            .revision
            .checked_add(1)
            .ok_or_else(|| CliFailure::new(1, "agent-route revision overflow"))?;
        existing.updated_at = Utc::now().to_rfc3339();
        return save_private_json(path, &existing, "agent route");
    }
    let now = Utc::now().to_rfc3339();
    let stored = StoredAgentRoute {
        schema_version: 2,
        route_id: route.route_id.clone(),
        owner_id: route.owner_id.clone(),
        origin_machine: route.origin_machine.clone(),
        agent: agent.clone(),
        revision: 1,
        created_at: now.clone(),
        updated_at: now,
    };
    save_private_json(path, &stored, "agent route")
}

fn acquire_agent_route_lock(path: &Path) -> Result<fs::File, CliFailure> {
    let parent = path
        .parent()
        .ok_or_else(|| CliFailure::new(1, "agent route path has no parent"))?;
    ensure_private_directory(parent)?;
    let lock_path = path.with_extension("lock");
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&lock_path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| CliFailure::new(1, format!("open agent route lock: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))
            .map_err(|error| CliFailure::new(1, format!("protect agent route lock: {error}")))?;
    }
    file.lock_exclusive()
        .map_err(|error| CliFailure::new(1, format!("lock agent route: {error}")))?;
    Ok(file)
}

fn same_immutable_agent_contract(
    existing: &AgentResumeContext,
    incoming: &AgentResumeContext,
) -> bool {
    existing.provider == incoming.provider
        && existing.session_id == incoming.session_id
        && existing.parent_session_id == incoming.parent_session_id
        && existing.resume_transport == incoming.resume_transport
        && match (&existing.terminal_provenance, &incoming.terminal_provenance) {
            (
                TerminalProvenance::Absent | TerminalProvenance::Cmux { .. },
                TerminalProvenance::Absent,
            )
            | (TerminalProvenance::Cmux { .. }, TerminalProvenance::Cmux { .. }) => true,
            (TerminalProvenance::Absent, TerminalProvenance::Cmux { surface_id }) => {
                existing.surface_id.as_deref() == Some(surface_id)
            }
            (left @ TerminalProvenance::HerdR { .. }, right) => left == right,
            _ => false,
        }
}

fn reconcile_surface_route(
    existing: &AgentResumeContext,
    incoming: &AgentResumeContext,
    allow_explicit_surface_change: bool,
) -> Result<AgentResumeContext, CliFailure> {
    let mut reconciled = existing.clone();
    match incoming.surface_provenance {
        SurfaceProvenance::Explicit => {
            if existing.surface_provenance == SurfaceProvenance::AmbientCmux
                && existing.surface_id == incoming.surface_id
            {
                reconciled.surface_provenance = SurfaceProvenance::Explicit;
            } else if allow_explicit_surface_change {
                reconciled.surface_id.clone_from(&incoming.surface_id);
                reconciled.surface_provenance = SurfaceProvenance::Explicit;
            } else if existing.surface_provenance != SurfaceProvenance::Explicit
                || existing.surface_id != incoming.surface_id
            {
                return Err(CliFailure::new(
                    1,
                    "explicit agent surface changed; explicit ownership transfer is required",
                ));
            }
        }
        SurfaceProvenance::AmbientCmux => {
            if existing.surface_provenance != SurfaceProvenance::Explicit {
                reconciled.surface_id.clone_from(&incoming.surface_id);
                reconciled.surface_provenance = SurfaceProvenance::AmbientCmux;
            }
        }
        SurfaceProvenance::Absent => {}
    }
    if let TerminalProvenance::Cmux { surface_id } = &incoming.terminal_provenance {
        reconciled.terminal_provenance = TerminalProvenance::Cmux {
            surface_id: surface_id.clone(),
        };
        // Live cmux evidence is refreshable for the same native session. The
        // immutable owner identity and generation stay unchanged; publication
        // later binds the newly observed process/surface tuple atomically.
        reconciled
            .terminal_authority
            .clone_from(&incoming.terminal_authority);
    }
    Ok(reconciled)
}

fn load_agent_route(path: &Path) -> Result<Option<StoredAgentRoute>, CliFailure> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| CliFailure::new(1, format!("invalid stored agent route: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CliFailure::new(
            1,
            format!("read stored agent route: {error}"),
        )),
    }
}

fn save_private_json<T: Serialize>(
    path: &Path,
    value: &T,
    description: &str,
) -> Result<(), CliFailure> {
    let parent = path
        .parent()
        .ok_or_else(|| CliFailure::new(1, format!("{description} path has no parent")))?;
    ensure_private_directory(parent)?;
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| CliFailure::new(1, format!("serialize {description}: {error}")))?;
    bytes.push(b'\n');
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        CliFailure::new(1, format!("create {description} temporary file: {error}"))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| CliFailure::new(1, format!("protect {description}: {error}")))?;
    }
    temporary
        .write_all(&bytes)
        .map_err(|error| CliFailure::new(1, format!("write {description}: {error}")))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| CliFailure::new(1, format!("sync {description}: {error}")))?;
    let temporary = temporary.into_temp_path();
    replace_file_with_windows_retry(&temporary, path)
        .map_err(|error| CliFailure::new(1, format!("publish {description}: {error}")))?;
    #[cfg(not(windows))]
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CliFailure::new(1, format!("sync {description} directory: {error}")))?;
    Ok(())
}

pub(super) fn verify_exact_open_pr(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    expected_head: &str,
) -> Result<(), CliFailure> {
    let value = gh_json(
        actions,
        &["api".to_owned(), format!("repos/{repo}/pulls/{pr}")],
        "pull-request handoff preflight",
    )
    .map_err(|error| CliFailure::new(1, error))?;
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if state != "open" {
        return Err(CliFailure::new(1, format!("PR #{pr} is not open")));
    }
    let current = value
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !current.eq_ignore_ascii_case(expected_head) {
        return Err(CliFailure::new(
            1,
            format!("PR #{pr} head drift: expected {expected_head}, current {current}"),
        ));
    }
    Ok(())
}

fn write_handoff_status(
    actions: &GitHubActions,
    repo: &str,
    args: &StewardHandoffArgs,
) -> Result<(), CliFailure> {
    let description = format!("Managed handoff {}", args.workstream_id);
    let mut command = vec![
        "api".to_owned(),
        "-X".to_owned(),
        "POST".to_owned(),
        format!("repos/{repo}/statuses/{}", args.head),
        "-f".to_owned(),
        "state=success".to_owned(),
        "-f".to_owned(),
        format!("context={HANDOFF_CONTEXT}"),
        "-f".to_owned(),
        format!("description={description}"),
    ];
    if let Some(url) = args.context_url.as_deref() {
        command.push("-f".to_owned());
        command.push(format!("target_url={url}"));
    }
    run_steward_write(actions, &command)
        .map_err(|error| CliFailure::new(1, format!("could not write handoff receipt: {error}")))?;
    Ok(())
}

fn handoff_status_is_present(
    actions: &GitHubActions,
    repo: &str,
    args: &StewardHandoffArgs,
) -> Result<bool, CliFailure> {
    let statuses = fetch_handoff_statuses(actions, repo, &args.head)?;
    let Some(status) = latest_handoff_status(&statuses)? else {
        return Ok(false);
    };
    let description = format!("Managed handoff {}", args.workstream_id);
    Ok(
        status.get("state").and_then(Value::as_str) == Some("success")
            && status.get("description").and_then(Value::as_str) == Some(description.as_str())
            && status.get("target_url").and_then(Value::as_str) == args.context_url.as_deref(),
    )
}

fn fetch_handoff_statuses(
    actions: &GitHubActions,
    repo: &str,
    head: &str,
) -> Result<Vec<Value>, CliFailure> {
    let mut statuses = Vec::new();
    for page in 1..=10 {
        let value = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!("repos/{repo}/commits/{head}/statuses?per_page=100&page={page}"),
            ],
            "handoff receipt reconciliation",
        )
        .map_err(|error| CliFailure::new(1, error))?;
        let rows = value.as_array().ok_or_else(|| {
            CliFailure::new(1, "handoff receipt reconciliation returned a non-array")
        })?;
        let count = rows.len();
        statuses.extend(rows.iter().cloned());
        if count < 100 {
            return Ok(statuses);
        }
    }
    Err(CliFailure::new(
        1,
        "handoff receipt reconciliation exceeds 1000 statuses; refusing partial scan",
    ))
}

fn latest_handoff_status(statuses: &[Value]) -> Result<Option<&Value>, CliFailure> {
    let mut matches = Vec::new();
    let mut seen_ids = BTreeSet::new();
    for status in statuses {
        let context = status
            .get("context")
            .and_then(Value::as_str)
            .ok_or_else(|| CliFailure::new(1, "commit status omitted string context"))?;
        if context != HANDOFF_CONTEXT {
            continue;
        }
        let created_at = status
            .get("created_at")
            .and_then(Value::as_str)
            .ok_or_else(|| CliFailure::new(1, "handoff status omitted created_at"))?;
        let timestamp = chrono::DateTime::parse_from_rfc3339(created_at)
            .map_err(|error| CliFailure::new(1, format!("invalid handoff status time: {error}")))?;
        let id = status
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| CliFailure::new(1, "handoff status omitted numeric id"))?;
        if !seen_ids.insert(id) {
            return Err(CliFailure::new(
                1,
                format!("handoff status repeated id {id}"),
            ));
        }
        let state = status
            .get("state")
            .and_then(Value::as_str)
            .ok_or_else(|| CliFailure::new(1, "handoff status omitted state"))?;
        if !matches!(state, "error" | "failure" | "pending" | "success") {
            return Err(CliFailure::new(
                1,
                format!("unknown handoff status `{state}`"),
            ));
        }
        matches.push((timestamp, id, status));
    }
    Ok(matches
        .into_iter()
        .max_by_key(|(timestamp, id, _)| (*timestamp, *id))
        .map(|(_, _, status)| status))
}

pub(super) fn ensure_label(
    actions: &GitHubActions,
    repo: &str,
    label: &str,
    color: &str,
    description: &str,
) -> Result<(), CliFailure> {
    let encoded = encode_path_segment(label);
    let inspect = actions.run_gh(&["api".to_owned(), format!("repos/{repo}/labels/{encoded}")]);
    match inspect {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("HTTP 404") => run_steward_write(
            actions,
            &[
                "api".to_owned(),
                "-X".to_owned(),
                "POST".to_owned(),
                format!("repos/{repo}/labels"),
                "-f".to_owned(),
                format!("name={label}"),
                "-f".to_owned(),
                format!("color={color}"),
                "-f".to_owned(),
                format!("description={description}"),
            ],
        )
        .map(|_| ())
        .map_err(|error| CliFailure::new(1, format!("could not create label: {error}"))),
        Err(error) => Err(CliFailure::new(
            1,
            format!("could not inspect managed label: {error}"),
        )),
    }
}

pub(super) fn add_label(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    label: &str,
) -> Result<(), CliFailure> {
    run_steward_write(
        actions,
        &[
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{repo}/issues/{pr}/labels"),
            "-f".to_owned(),
            format!("labels[]={label}"),
        ],
    )
    .map(|_| ())
    .map_err(|error| CliFailure::new(1, format!("could not add label {label}: {error}")))
}

pub(super) fn remove_label(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    label: &str,
) -> Result<(), CliFailure> {
    let encoded = encode_path_segment(label);
    match run_steward_write(
        actions,
        &[
            "api".to_owned(),
            "-X".to_owned(),
            "DELETE".to_owned(),
            format!("repos/{repo}/issues/{pr}/labels/{encoded}"),
        ],
    ) {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("HTTP 404") => Ok(()),
        Err(error) => Err(CliFailure::new(
            1,
            format!("could not remove label {label}: {error}"),
        )),
    }
}

pub(super) fn run_steward_write(
    actions: &GitHubActions,
    args: &[String],
) -> Result<String, crate::cloud::GitHubError> {
    actions.run_gh(args)
}

fn render<W: Write>(
    args: &StewardHandoffArgs,
    repo: &str,
    agent_route: Option<&AgentRouteReference>,
    origin_machine: &str,
    json_output: bool,
    wake_consumer_available: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json_output {
        let data = render_json_data(
            args,
            repo,
            agent_route,
            origin_machine,
            wake_consumer_available,
        )?;
        return write_json_envelope(stdout, "runner.steward-handoff", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "steward handoff: mode={} repo={} pr=#{} head={} workstream={} label={} monitoring_transferred={} wake_consumer_available={} origin_machine={} repair_route={}",
        if args.apply { "apply" } else { "dry-run" },
        repo,
        args.pr,
        args.head,
        args.workstream_id,
        MANAGED_LABEL,
        wake_consumer_available,
        wake_consumer_available,
        origin_machine,
        if agent_route.is_some() {
            "original_agent"
        } else {
            "fresh_agent_only"
        }
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn render_json_data(
    args: &StewardHandoffArgs,
    repo: &str,
    agent_route: Option<&AgentRouteReference>,
    origin_machine: &str,
    wake_consumer_available: bool,
) -> Result<BTreeMap<String, Value>, CliFailure> {
    let mut data = BTreeMap::from([
        ("apply".to_owned(), Value::from(args.apply)),
        ("repo".to_owned(), Value::from(repo)),
        ("pr".to_owned(), Value::from(args.pr)),
        ("head_sha".to_owned(), Value::from(args.head.clone())),
        (
            "workstream_id".to_owned(),
            Value::from(args.workstream_id.clone()),
        ),
        ("managed_label".to_owned(), Value::from(MANAGED_LABEL)),
        ("handoff_context".to_owned(), Value::from(HANDOFF_CONTEXT)),
        (
            "monitoring_transferred".to_owned(),
            Value::from(wake_consumer_available),
        ),
        (
            "wake_consumer_available".to_owned(),
            Value::from(wake_consumer_available),
        ),
        (
            "origin_machine".to_owned(),
            Value::from(origin_machine.to_owned()),
        ),
        (
            "repair_route".to_owned(),
            Value::from(if agent_route.is_some() {
                "original_agent"
            } else {
                "fresh_agent_only"
            }),
        ),
    ]);
    if let Some(agent_route) = agent_route {
        data.insert(
            "agent_route".to_owned(),
            serde_json::to_value(agent_route)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
    }
    if let Some(url) = args.context_url.as_deref() {
        data.insert("context_url".to_owned(), Value::from(url));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn sequenced_gh(
        temp: &tempfile::TempDir,
        first_error: &str,
    ) -> (GitHubActions, std::path::PathBuf) {
        let count = temp.path().join("count");
        let source = format!(
            r#"
	use std::path::Path;

fn main() {{
    let path = Path::new({count:?});
    let previous = std::fs::read_to_string(path)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let current = previous + 1;
    std::fs::write(path, current.to_string()).expect("write invocation count");
    if current == 1 {{
        eprintln!({first_error:?});
        std::process::exit(1);
    }}
    println!("{{{{}}}}");
}}
"#,
            count = count.to_string_lossy(),
        );
        let binary = crate::test_support::compile_native_test_program(temp.path(), "gh", &source);
        (
            GitHubActions::new(temp.path()).with_gh_binary_for_tests(binary),
            count,
        )
    }

    #[cfg(unix)]
    fn handoff_status_failing_gh(
        temp: &tempfile::TempDir,
        head: &str,
    ) -> (GitHubActions, std::path::PathBuf) {
        let count = temp.path().join("handoff-count");
        let pull_json = serde_json::json!({
            "state": "open",
            "head": {"sha": head},
        })
        .to_string();
        let source = format!(
            r#"
	use std::path::Path;
	use std::io::Write as _;

fn main() {{
    let count_path = Path::new({count:?});
    let previous = std::fs::read_to_string(count_path)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    std::fs::write(count_path, (previous + 1).to_string()).expect("write count");
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "repos/owner/repo/pulls/7") {{
        std::io::stdout()
            .write_all({pull_json:?}.as_bytes())
            .expect("write pull response");
        return;
    }}
    if args.iter().any(|arg| arg.starts_with("repos/owner/repo/commits/") && arg.contains("/statuses?")) {{
        println!("[]");
        return;
    }}
    if args.iter().any(|arg| arg.starts_with("repos/owner/repo/statuses/")) {{
        eprintln!("HTTP 403 generic forbidden");
        std::process::exit(1);
    }}
    println!("{{{{}}}}");
}}
"#,
            count = count.to_string_lossy(),
        );
        let binary =
            crate::test_support::compile_native_test_program(temp.path(), "handoff-gh", &source);
        (
            GitHubActions::new(temp.path()).with_gh_binary_for_tests(binary),
            count,
        )
    }

    #[cfg(unix)]
    fn handoff_success_gh(
        temp: &tempfile::TempDir,
        head: &str,
        statuses_json: &str,
    ) -> (GitHubActions, std::path::PathBuf) {
        let log = temp.path().join("handoff-gh.log");
        let pull_json = serde_json::json!({
            "state": "open",
            "head": {"sha": head},
        })
        .to_string();
        let source = format!(
            r#"
use std::io::Write as _;
use std::path::Path;

fn main() {{
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(Path::new({log:?}))
        .expect("open log");
    writeln!(log, "{{}}", args.join("\t")).expect("write log");
    if args.iter().any(|arg| arg == "repos/owner/repo/pulls/7") {{
        std::io::stdout()
            .write_all({pull_json:?}.as_bytes())
            .expect("write pull response");
        return;
    }}
    if args.iter().any(|arg| arg.starts_with("repos/owner/repo/commits/") && arg.contains("/statuses?")) {{
        std::io::stdout()
            .write_all({statuses_json:?}.as_bytes())
            .expect("write statuses response");
        return;
    }}
    println!("{{{{}}}}");
}}
"#,
            log = log.to_string_lossy(),
        );
        let binary =
            crate::test_support::compile_native_test_program(temp.path(), "handoff-ok-gh", &source);
        (
            GitHubActions::new(temp.path()).with_gh_binary_for_tests(binary),
            log,
        )
    }

    fn args() -> StewardHandoffArgs {
        StewardHandoffArgs {
            repo: Some("owner/repo".to_owned()),
            pr: 7,
            head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            workstream_id: "GEN-7".to_owned(),
            context_url: Some("https://linear.app/example/GEN-7".to_owned()),
            agent_provider: None,
            agent_session_id: None,
            agent_parent_session_id: None,
            agent_surface_id: None,
            transfer_agent_owner: false,
            apply: false,
        }
    }

    fn explicit_agent_args(provider: &str, session_id: &str) -> StewardHandoffArgs {
        let mut value = args();
        value.agent_provider = Some(provider.to_owned());
        value.agent_session_id = Some(session_id.to_owned());
        value
    }

    fn route_for(args: &StewardHandoffArgs, origin: &str) -> AgentRouteReference {
        let agent = resolve_agent_context_with_environment(args, &AgentEnvironment::default())
            .expect("resolve agent")
            .expect("agent route");
        agent_route_reference(&agent, origin)
    }

    #[test]
    fn rejects_non_exact_head_and_non_http_context_before_transport() {
        let mut invalid = args();
        invalid.head = "abc".to_owned();
        assert!(validate_args(&invalid).is_err());
        invalid = args();
        invalid.context_url = Some("file:///tmp/private".to_owned());
        assert!(validate_args(&invalid).is_err());
    }

    /// A mixed-case owner must not make the fallback unreachable.
    ///
    /// The hatch canonicalises on a lowercase slug. It also used to require the
    /// slug to ALREADY be lowercase, which meant any repository whose owner or
    /// name carries a capital -- `Generous-Corp/pulp`, and most repos -- could
    /// never take the fallback. `shipyard pr` then pushed the branch and refused
    /// the handoff, leaving an unowned PR.
    #[test]
    fn legacy_pr_fallback_accepts_a_mixed_case_repository_slug() {
        let mut mixed = args();
        mixed.repo = Some("Generous-Corp/pulp".to_owned());
        mixed.pr = 7;
        // Exactly what ship_cmd::provenance synthesizes.
        mixed.workstream_id = "generous-corp/pulp#7".to_owned();
        assert!(
            is_legacy_pr_fallback(&mixed),
            "a mixed-case slug must still reach the legacy fallback"
        );
        assert!(validate_args(&mixed).is_ok());

        // CONTROL: the hatch is still exact about the PR number, so it cannot be
        // satisfied by any id that merely looks similar.
        let mut wrong_pr = mixed;
        wrong_pr.workstream_id = "generous-corp/pulp#8".to_owned();
        assert!(!is_legacy_pr_fallback(&wrong_pr));
    }

    #[test]
    fn legacy_pr_fallback_refuses_ambient_agent_routes_and_managed_lifecycles() {
        let mut legacy = args();
        legacy.workstream_id = "owner/repo#7".to_owned();
        assert!(validate_args(&legacy).is_ok());

        let absent = resolve_handoff_agent(&legacy, |args| {
            resolve_agent_context_with_environment(args, &AgentEnvironment::default())
        })
        .expect("resolve profile-free legacy context");
        assert!(absent.is_none());

        let environments = [
            (
                "ambient Codex",
                AgentEnvironment {
                    codex_session: Some("codex-session".to_owned()),
                    ..AgentEnvironment::default()
                },
            ),
            (
                "ambient Claude",
                AgentEnvironment {
                    claude_session: Some("claude-session".to_owned()),
                    ..AgentEnvironment::default()
                },
            ),
            (
                "ambient HerdR Codex",
                AgentEnvironment {
                    codex_session: Some("codex-herdr".to_owned()),
                    herdr_env: Some("1".to_owned()),
                    herdr_session: Some("herdr-session".to_owned()),
                    herdr_workspace_id: Some("workspace".to_owned()),
                    herdr_tab_id: Some("tab".to_owned()),
                    herdr_pane_id: Some("pane".to_owned()),
                    ..AgentEnvironment::default()
                },
            ),
            (
                "ambient cmux Codex",
                AgentEnvironment {
                    codex_session: Some("codex-cmux".to_owned()),
                    surface_id: Some("surface".to_owned()),
                    ..AgentEnvironment::default()
                },
            ),
        ];
        for (name, environment) in environments {
            let error = resolve_handoff_agent(&legacy, |args| {
                resolve_agent_context_with_environment(args, &environment)
            })
            .expect_err("ambient route must refuse legacy fallback");
            assert!(
                error
                    .message()
                    .contains("cannot bind an agent route or managed lifecycle"),
                "unexpected {name} refusal: {}",
                error.message()
            );
        }
    }

    #[test]
    fn agent_identity_requires_a_complete_provider_session_pair() {
        let mut partial = args();
        partial.agent_provider = Some("codex".to_owned());
        assert!(resolve_agent_context(&partial).is_err());

        partial.agent_session_id = Some("019d-test-thread".to_owned());
        let context =
            resolve_agent_context_with_environment(&partial, &AgentEnvironment::default())
                .expect("valid context")
                .expect("captured context");
        assert_eq!(context.provider, "codex");
        assert_eq!(context.resume_transport, "codex_queue");
    }

    #[test]
    fn herdr_route_uses_real_environment_contract_and_private_agent_provenance() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let agent_args = explicit_agent_args("codex", "provider-session-7");
        let environment = AgentEnvironment {
            herdr_env: Some("1".to_owned()),
            herdr_session: Some("herdr-session-1".to_owned()),
            herdr_workspace_id: Some("workspace-2".to_owned()),
            herdr_tab_id: Some("tab-3".to_owned()),
            herdr_pane_id: Some("pane-4".to_owned()),
            ..AgentEnvironment::default()
        };
        let no_agent = args();
        assert!(
            resolve_agent_context_with_environment(&no_agent, &environment)
                .expect_err("HerdR route cannot be discarded without an agent session")
                .message()
                .contains("resumable agent session")
        );
        let agent = resolve_agent_context_with_environment(&agent_args, &environment)
            .expect("typed HerdR route")
            .expect("agent");
        assert_eq!(
            agent.terminal_provenance,
            TerminalProvenance::HerdR {
                session_id: "herdr-session-1".to_owned(),
                workspace_id: "workspace-2".to_owned(),
                tab_id: "tab-3".to_owned(),
                pane_id: "pane-4".to_owned(),
                provider_session_id: "provider-session-7".to_owned(),
            }
        );
        let route = agent_route_reference(&agent, "m3");
        assert_eq!(route.terminal_provenance, TerminalProvenanceKind::HerdR);
        let route_path = agent_route_path(&paths, &route.route_id);
        persist_agent_route(&route_path, &route, &agent).expect("durable private route");
        let durable = load_agent_route(&route_path)
            .expect("load route")
            .expect("stored route");
        assert_eq!(durable.agent.terminal_provenance, agent.terminal_provenance);
        let private_json = std::fs::read_to_string(route_path).expect("private route JSON");
        for identity in [
            "herdr-session-1",
            "workspace-2",
            "tab-3",
            "pane-4",
            "provider-session-7",
        ] {
            assert!(private_json.contains(identity), "missing {identity}");
        }
        let receipt_path = handoff_path(
            &handoff_directory(&paths, "owner/repo", agent_args.pr),
            &agent_args.head,
        );
        let receipt =
            prepare_handoff_receipt(None, &agent_args, "owner/repo", "m3", Some(route.clone()))
                .expect("HerdR receipt");
        persist_handoff(&receipt_path, receipt, HandoffPhase::Managed).expect("managed receipt");
        let terminal = terminal_owner_route(
            &paths.state_dir,
            "owner/repo",
            agent_args.pr,
            &agent_args.head,
        )
        .expect("valid route")
        .expect("terminal owner");
        assert_eq!(
            terminal.terminal_provenance,
            Some(TerminalProvenanceKind::HerdR)
        );
        let public_json = std::fs::read_to_string(receipt_path).expect("public receipt JSON");
        for private_identity in [
            "herdr-session-1",
            "workspace-2",
            "tab-3",
            "pane-4",
            "provider-session-7",
        ] {
            assert!(!public_json.contains(private_identity));
        }
    }

    #[test]
    fn herdr_route_defaults_the_absent_optional_session_name() {
        let agent_args = explicit_agent_args("claude", "provider-session-8");
        let environment = AgentEnvironment {
            herdr_env: Some("1".to_owned()),
            herdr_workspace_id: Some("workspace-2".to_owned()),
            herdr_tab_id: Some("tab-3".to_owned()),
            herdr_pane_id: Some("pane-4".to_owned()),
            ..AgentEnvironment::default()
        };
        let agent = resolve_agent_context_with_environment(&agent_args, &environment)
            .expect("default HerdR session route")
            .expect("agent");
        assert_eq!(
            agent.terminal_provenance,
            TerminalProvenance::HerdR {
                session_id: "default".to_owned(),
                workspace_id: "workspace-2".to_owned(),
                tab_id: "tab-3".to_owned(),
                pane_id: "pane-4".to_owned(),
                provider_session_id: "provider-session-8".to_owned(),
            }
        );
    }

    #[test]
    fn herdr_route_rejects_partial_unmarked_and_conflicting_inputs() {
        let agent_args = explicit_agent_args("codex", "provider-session-7");
        let complete = AgentEnvironment {
            herdr_env: Some("1".to_owned()),
            herdr_session: Some("herdr-session-1".to_owned()),
            herdr_workspace_id: Some("workspace-2".to_owned()),
            herdr_tab_id: Some("tab-3".to_owned()),
            herdr_pane_id: Some("pane-4".to_owned()),
            ..AgentEnvironment::default()
        };
        let mut partial = complete.clone();
        partial.herdr_pane_id = None;
        assert!(
            resolve_agent_context_with_environment(&agent_args, &partial)
                .expect_err("partial route")
                .message()
                .contains("requires workspace, tab, and pane")
        );

        let mut unmarked = complete.clone();
        unmarked.herdr_env = None;
        assert!(
            resolve_agent_context_with_environment(&agent_args, &unmarked)
                .expect_err("unmarked HerdR fields are unknown route input")
                .message()
                .contains("HERDR_ENV=1")
        );

        let mut wrong_marker = complete.clone();
        wrong_marker.herdr_env = Some("true".to_owned());
        assert!(
            resolve_agent_context_with_environment(&agent_args, &wrong_marker)
                .expect_err("non-literal marker")
                .message()
                .contains("exactly 1")
        );

        let mut conflicting = complete;
        conflicting.surface_id = Some("cmux-surface".to_owned());
        assert!(
            resolve_agent_context_with_environment(&agent_args, &conflicting)
                .expect_err("HerdR and cmux routes conflict")
                .message()
                .contains("cannot be combined")
        );
    }

    #[test]
    fn ambiguous_provider_environment_and_explicit_orphan_route_fields_fail_closed() {
        let environment = AgentEnvironment {
            codex_session: Some("codex-session".to_owned()),
            claude_session: Some("claude-session".to_owned()),
            surface_id: None,
            ..AgentEnvironment::default()
        };
        let error = resolve_agent_context_with_environment(&args(), &environment)
            .expect_err("ambiguous providers must fail");
        assert!(error.message().contains("both Codex and Claude"));

        let mut explicit = explicit_agent_args("codex", "explicit-session");
        assert!(resolve_agent_context_with_environment(&explicit, &environment).is_ok());

        explicit.agent_provider = None;
        explicit.agent_session_id = None;
        let ambient_surface = AgentEnvironment {
            surface_id: Some("surface-without-session".to_owned()),
            ..AgentEnvironment::default()
        };
        assert!(
            resolve_agent_context_with_environment(&explicit, &ambient_surface)
                .expect("ambient cmux surface is advisory")
                .is_none()
        );

        explicit.agent_surface_id = Some("explicit-surface-without-session".to_owned());
        let error = resolve_agent_context_with_environment(&explicit, &AgentEnvironment::default())
            .expect_err("explicit orphan surface must fail");
        assert!(error.message().contains("parent/surface"));

        explicit.agent_surface_id = None;
        explicit.agent_parent_session_id = Some("parent-without-session".to_owned());
        let error = resolve_agent_context_with_environment(&explicit, &AgentEnvironment::default())
            .expect_err("explicit orphan parent must fail");
        assert!(error.message().contains("parent/surface"));
    }

    #[test]
    fn handoff_repository_directory_encoding_is_collision_free() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let first = handoff_directory(&paths, "a--b/c", 42);
        let second = handoff_directory(&paths, "a/b--c", 42);
        assert_ne!(first, second);
        assert!(first.ends_with("a--b%2Fc/pr-42"));
        assert!(second.ends_with("a%2Fb--c/pr-42"));
    }

    #[test]
    fn no_agent_route_is_explicitly_fresh_agent_only() {
        let handoff = prepare_handoff_receipt(None, &args(), "owner/repo", "m3", None)
            .expect("fresh-agent handoff");
        assert_eq!(handoff.owner_id, "fresh-agent-only");
        assert_eq!(handoff.repair_route, RepairRoute::FreshAgentOnly);
        assert_eq!(handoff.agent_route, None);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "privacy, restart, tamper, and permission assertions form one lifecycle scenario"
    )]
    fn durable_handoff_is_private_exact_head_scoped_and_restart_monotonic() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut managed = args();
        managed.agent_provider = Some("claude".to_owned());
        managed.agent_session_id = Some("session-7".to_owned());
        managed.agent_parent_session_id = Some("coordinator-1".to_owned());
        managed.agent_surface_id = Some("surface-7".to_owned());
        let agent = resolve_agent_context_with_environment(&managed, &AgentEnvironment::default())
            .expect("agent context")
            .expect("captured agent");
        let route = agent_route_reference(&agent, "m3");
        let directory = handoff_directory(&paths, "owner/repo", managed.pr);
        let path = handoff_path(&directory, &managed.head);
        let route_path = agent_route_path(&paths, &route.route_id);
        persist_agent_route(&route_path, &route, &agent).expect("persist private route");
        let candidate =
            prepare_handoff_receipt(None, &managed, "owner/repo", "m3", Some(route.clone()))
                .expect("prepare handoff");
        let intent =
            persist_handoff(&path, candidate, HandoffPhase::Intent).expect("persist intent");
        assert_eq!(intent.phase, HandoffPhase::Intent);
        assert_eq!(intent.revision, 1);
        assert_eq!(intent.ownership_generation, 1);
        let ready = persist_handoff(&path, intent, HandoffPhase::Ready).expect("persist ready");
        assert_eq!(ready.phase, HandoffPhase::Ready);
        assert_eq!(ready.revision, 2);

        let loaded = load_handoff(&path).expect("load receipt").expect("receipt");
        let replay =
            prepare_handoff_receipt(Some(loaded), &managed, "owner/repo", "m3", Some(route))
                .expect("same-owner replay");
        let replayed_intent =
            persist_handoff(&path, replay, HandoffPhase::Intent).expect("persist replay intent");
        assert_eq!(replayed_intent.phase, HandoffPhase::Ready);
        assert_eq!(replayed_intent.revision, 3);
        assert_eq!(replayed_intent.ownership_generation, 1);
        assert_eq!(
            terminal_owner_route(&paths.state_dir, "owner/repo", managed.pr, &managed.head)
                .expect("ready receipt is valid but not wake authority"),
            None
        );
        let receipt = persist_handoff(&path, replayed_intent, HandoffPhase::Managed)
            .expect("persist managed");
        assert_eq!(receipt.phase, HandoffPhase::Managed);
        assert_eq!(receipt.revision, 4);
        assert_eq!(receipt.head_sha, managed.head);
        assert!(!receipt.wake_consumer_available);
        let terminal_owner =
            terminal_owner_route(&paths.state_dir, "owner/repo", managed.pr, &managed.head)
                .expect("read terminal owner after restart")
                .expect("terminal owner");
        assert_eq!(terminal_owner.origin_machine, "m3");
        assert_eq!(
            terminal_owner.owner_id,
            opaque_id("owner", &["claude", "coordinator-1"])
        );
        assert_ne!(terminal_owner.owner_id, receipt.owner_id);
        assert_eq!(
            terminal_owner.route_id.as_deref(),
            receipt
                .agent_route
                .as_ref()
                .map(|route| route.route_id.as_str())
        );
        assert_eq!(terminal_owner.provider.as_deref(), Some("claude"));
        assert_eq!(
            terminal_owner.resume_transport.as_deref(),
            Some("claude_resume")
        );
        assert_eq!(
            terminal_owner.terminal_provenance,
            Some(TerminalProvenanceKind::Cmux)
        );

        let public_bytes = std::fs::read_to_string(&path).expect("read receipt");
        assert!(!public_bytes.contains("session-7"));
        assert!(!public_bytes.contains("coordinator-1"));
        assert!(!public_bytes.contains("surface-7"));
        let private_bytes = std::fs::read_to_string(&route_path).expect("read route");
        assert!(private_bytes.contains("session-7"));
        assert!(private_bytes.contains("coordinator-1"));
        assert!(private_bytes.contains("surface-7"));

        let mut tampered = load_agent_route(&route_path)
            .expect("load route")
            .expect("stored route");
        tampered.agent.parent_session_id = Some("attacker-session".to_owned());
        save_private_json(&route_path, &tampered, "tampered test route").expect("tamper route");
        assert!(
            terminal_owner_route(&paths.state_dir, "owner/repo", managed.pr, &managed.head)
                .expect_err("tampered coordinator identity must fail")
                .message
                .contains("identity disagree")
        );
        let unresolved = terminal_owner_route_or_unresolved(
            &paths.state_dir,
            "owner/repo",
            managed.pr,
            &managed.head,
        )
        .expect("retain exact origin without trusting tampered route");
        assert_eq!(unresolved.origin_machine, "m3");
        assert_eq!(unresolved.owner_disposition, "unroutable_private_route");
        assert_eq!(unresolved.route_id, None);

        let valid_receipt = load_handoff(&path)
            .expect("load receipt")
            .expect("stored receipt");
        let mut zero_generation = valid_receipt.clone();
        zero_generation.ownership_generation = 0;
        let mut zero_revision = valid_receipt.clone();
        zero_revision.revision = 0;
        let mut enabled_consumer = valid_receipt.clone();
        enabled_consumer.wake_consumer_available = true;
        let mut inconsistent_repair = valid_receipt;
        inconsistent_repair.repair_route = RepairRoute::FreshAgentOnly;
        for (case, invalid_receipt) in [
            ("zero generation", zero_generation),
            ("zero revision", zero_revision),
            ("enabled consumer", enabled_consumer),
            ("inconsistent repair", inconsistent_repair),
        ] {
            save_private_json(&path, &invalid_receipt, "invalid test receipt")
                .expect("tamper receipt");
            assert!(
                terminal_owner_route(&paths.state_dir, "owner/repo", managed.pr, &managed.head)
                    .is_err(),
                "{case}"
            );
            assert_eq!(
                terminal_owner_route_or_unresolved(
                    &paths.state_dir,
                    "owner/repo",
                    managed.pr,
                    &managed.head
                ),
                None,
                "{case}"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("receipt metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&route_path)
                    .expect("route metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().expect("receipt parent"))
                    .expect("directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn conflicting_owner_or_route_requires_explicit_transfer() {
        let first = explicit_agent_args("codex", "session-one");
        let first_route = route_for(&first, "m3");
        let receipt =
            prepare_handoff_receipt(None, &first, "owner/repo", "m3", Some(first_route.clone()))
                .expect("first handoff");
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("receipt.json");
        let receipt =
            persist_handoff(&path, receipt, HandoffPhase::Intent).expect("persist first owner");

        let second = explicit_agent_args("codex", "session-two");
        let second_route = route_for(&second, "m3");
        let error = prepare_handoff_receipt(
            Some(receipt.clone()),
            &second,
            "owner/repo",
            "m3",
            Some(second_route),
        )
        .expect_err("different owner must fail");
        assert!(error.message().contains("explicit ownership transfer"));

        let mut changed_route_args = first.clone();
        changed_route_args.agent_parent_session_id = Some("different-parent".to_owned());
        let changed_route = route_for(&changed_route_args, "m3");
        let error = prepare_handoff_receipt(
            Some(receipt.clone()),
            &changed_route_args,
            "owner/repo",
            "m3",
            Some(changed_route),
        )
        .expect_err("same owner with changed route must fail");
        assert!(error.message().contains("route metadata changed"));

        let origin_route = route_for(&first, "m5");
        let error = prepare_handoff_receipt(
            Some(receipt),
            &first,
            "owner/repo",
            "m5",
            Some(origin_route),
        )
        .expect_err("same owner from a different origin must fail");
        assert!(
            error.message().contains("route metadata changed")
                || error.message().contains("origin machine changed")
        );
    }

    #[test]
    fn explicit_replacement_owner_increments_generation_without_changing_work() {
        let first = explicit_agent_args("codex", "expired-session");
        let first_route = route_for(&first, "m3");
        let receipt = prepare_handoff_receipt(None, &first, "owner/repo", "m3", Some(first_route))
            .expect("first handoff");
        let temp = tempfile::tempdir().expect("temp");
        let receipt = persist_handoff(
            &temp.path().join("receipt.json"),
            receipt,
            HandoffPhase::Ready,
        )
        .expect("persist first owner");
        let created_at = receipt.created_at.clone();

        let mut replacement = explicit_agent_args("claude", "replacement-session");
        replacement.transfer_agent_owner = true;
        let replacement_route = route_for(&replacement, "m5");
        let transferred = prepare_handoff_receipt(
            Some(receipt),
            &replacement,
            "owner/repo",
            "m5",
            Some(replacement_route.clone()),
        )
        .expect("explicit transfer");

        assert_eq!(transferred.owner_id, replacement_route.owner_id);
        assert_eq!(transferred.agent_route, Some(replacement_route.clone()));
        assert_eq!(transferred.origin_machine, "m5");
        assert_eq!(transferred.ownership_generation, 2);
        assert_eq!(transferred.phase, HandoffPhase::Ready);
        assert_eq!(transferred.created_at, created_at);
        assert_eq!(transferred.workstream_id, "GEN-7");

        let replayed = prepare_handoff_receipt(
            Some(transferred),
            &replacement,
            "owner/repo",
            "m5",
            Some(replacement_route),
        )
        .expect("replacement transfer replay");
        assert_eq!(replayed.ownership_generation, 2);
    }

    #[test]
    fn transfer_requires_explicit_replacement_session_and_existing_receipt() {
        let mut transfer = args();
        transfer.transfer_agent_owner = true;
        let error = validate_args(&transfer).expect_err("ambient transfer must fail");
        assert!(error.message().contains("explicit --agent-provider"));

        let transfer = {
            let mut value = explicit_agent_args("codex", "replacement-session");
            value.transfer_agent_owner = true;
            value
        };
        let error = prepare_handoff_receipt(
            None,
            &transfer,
            "owner/repo",
            "m3",
            Some(route_for(&transfer, "m3")),
        )
        .expect_err("transfer without receipt must fail");
        assert!(error.message().contains("existing exact-head"));
    }

    #[test]
    fn persisted_machine_identity_does_not_drift_when_machine_tag_changes() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        std::fs::create_dir_all(&paths.state_dir).expect("state directory");
        std::fs::write(paths.state_dir.join("machine-tag"), "m3\n").expect("machine tag");
        assert_eq!(
            resolve_origin_machine(&paths).expect("first identity"),
            "m3"
        );

        std::fs::write(paths.state_dir.join("machine-tag"), "m5\n").expect("changed tag");
        assert_eq!(
            resolve_origin_machine(&paths).expect("persisted identity"),
            "m3"
        );
    }

    #[test]
    fn missing_machine_tag_creates_one_stable_opaque_identity() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let first = resolve_origin_machine(&paths).expect("generated identity");
        let second = resolve_origin_machine(&paths).expect("reloaded identity");
        assert!(first.starts_with("machine-"));
        assert_eq!(first, second);
    }

    #[test]
    fn oversized_machine_tag_is_rejected_before_identity_is_persisted() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        std::fs::create_dir_all(&paths.state_dir).expect("state directory");
        std::fs::write(paths.state_dir.join("machine-tag"), "m".repeat(257)).expect("machine tag");
        let error = resolve_origin_machine(&paths).expect_err("oversized identity must fail");
        assert!(error.message().contains("origin machine"));
        assert!(!paths.state_dir.join("machine-identity.json").exists());
    }

    #[test]
    fn machine_identity_preview_is_read_only_before_apply() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        assert_eq!(
            preview_origin_machine(&paths).expect("preview identity"),
            "unpersisted-machine"
        );
        assert!(!paths.state_dir.exists());
    }

    #[test]
    fn ambient_cmux_surface_reconciles_without_changing_owner_or_route() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let agent_args = explicit_agent_args("codex", "stable-session");
        let first_environment = AgentEnvironment {
            surface_id: Some("surface-one".to_owned()),
            ..AgentEnvironment::default()
        };
        let first_agent = resolve_agent_context_with_environment(&agent_args, &first_environment)
            .expect("first context")
            .expect("first agent");
        assert_eq!(
            first_agent.surface_provenance,
            SurfaceProvenance::AmbientCmux
        );
        let first_route = agent_route_reference(&first_agent, "m3");
        let route_path = agent_route_path(&paths, &first_route.route_id);
        persist_agent_route(&route_path, &first_route, &first_agent).expect("first route");

        let second_environment = AgentEnvironment {
            surface_id: Some("surface-two".to_owned()),
            ..AgentEnvironment::default()
        };
        let second_agent = resolve_agent_context_with_environment(&agent_args, &second_environment)
            .expect("second context")
            .expect("second agent");
        let second_route = agent_route_reference(&second_agent, "m3");
        assert_eq!(first_route, second_route);
        let handoff = prepare_handoff_receipt(
            None,
            &agent_args,
            "owner/repo",
            "m3",
            Some(first_route.clone()),
        )
        .expect("first handoff");
        let handoff_path = temp.path().join("handoff.json");
        let handoff =
            persist_handoff(&handoff_path, handoff, HandoffPhase::Intent).expect("persist handoff");
        let replay = prepare_handoff_receipt(
            Some(handoff),
            &agent_args,
            "owner/repo",
            "m3",
            Some(second_route.clone()),
        )
        .expect("ambient surface change preserves handoff owner");
        assert_eq!(replay.owner_id, first_route.owner_id);
        assert_eq!(replay.ownership_generation, 1);
        persist_agent_route(&route_path, &second_route, &second_agent).expect("reconcile route");

        let stored = load_agent_route(&route_path)
            .expect("load route")
            .expect("stored route");
        assert_eq!(stored.owner_id, first_route.owner_id);
        assert_eq!(stored.route_id, first_route.route_id);
        assert_eq!(stored.origin_machine, "m3");
        assert_eq!(stored.agent.provider, "codex");
        assert_eq!(stored.agent.session_id, "stable-session");
        assert_eq!(stored.agent.surface_id.as_deref(), Some("surface-two"));
        assert_eq!(
            stored.agent.surface_provenance,
            SurfaceProvenance::AmbientCmux
        );
        assert_eq!(stored.revision, 2);

        let mut pinned_args = agent_args;
        pinned_args.agent_surface_id = Some("surface-two".to_owned());
        let pinned_agent =
            resolve_agent_context_with_environment(&pinned_args, &AgentEnvironment::default())
                .expect("pinned context")
                .expect("pinned agent");
        let pinned_route = agent_route_reference(&pinned_agent, "m3");
        assert_eq!(pinned_route, second_route);
        persist_agent_route(&route_path, &pinned_route, &pinned_agent)
            .expect("pin identical ambient surface");
        let pinned = load_agent_route(&route_path)
            .expect("load pinned route")
            .expect("pinned route");
        assert_eq!(pinned.agent.surface_provenance, SurfaceProvenance::Explicit);
        assert_eq!(pinned.agent.surface_id.as_deref(), Some("surface-two"));
        assert_eq!(pinned.revision, 3);
    }

    #[test]
    fn legacy_cmux_route_without_typed_provenance_upgrades_on_replay() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let agent_args = explicit_agent_args("codex", "legacy-cmux-session");
        let environment = AgentEnvironment {
            surface_id: Some("legacy-surface".to_owned()),
            ..AgentEnvironment::default()
        };
        let agent = resolve_agent_context_with_environment(&agent_args, &environment)
            .expect("agent context")
            .expect("agent");
        let route = agent_route_reference(&agent, "m3");
        assert_eq!(
            route.route_id,
            "route-e5c34af7af87a08e42cd9b47ff6487a331dd64e2bee59b943dded14873e298cf",
            "Absent/cmux routes must retain the pre-provenance hash contract"
        );
        let route_path = agent_route_path(&paths, &route.route_id);
        persist_agent_route(&route_path, &route, &agent).expect("current route");

        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&route_path).expect("read current route"))
                .expect("route JSON");
        legacy["agent"]
            .as_object_mut()
            .expect("agent object")
            .remove("terminal_provenance");
        std::fs::write(
            &route_path,
            serde_json::to_vec_pretty(&legacy).expect("legacy JSON"),
        )
        .expect("write legacy route");

        persist_agent_route(&route_path, &route, &agent)
            .expect("legacy route replay must upgrade in place");
        let upgraded = load_agent_route(&route_path)
            .expect("load upgraded route")
            .expect("stored route");
        assert_eq!(upgraded.route_id, route.route_id);
        assert_eq!(upgraded.owner_id, route.owner_id);
        assert_eq!(upgraded.revision, 2);
        assert_eq!(
            upgraded.agent.terminal_provenance,
            TerminalProvenance::Cmux {
                surface_id: "legacy-surface".to_owned(),
            }
        );
    }

    #[test]
    fn explicit_surface_change_remains_fenced() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut first_args = explicit_agent_args("codex", "stable-session");
        first_args.agent_surface_id = Some("pinned-one".to_owned());
        let first_agent =
            resolve_agent_context_with_environment(&first_args, &AgentEnvironment::default())
                .expect("first context")
                .expect("first agent");
        let first_route = agent_route_reference(&first_agent, "m3");
        let route_path = agent_route_path(&paths, &first_route.route_id);
        persist_agent_route(&route_path, &first_route, &first_agent).expect("first route");

        let mut changed_args = first_args;
        changed_args.agent_surface_id = Some("pinned-two".to_owned());
        let changed_agent =
            resolve_agent_context_with_environment(&changed_args, &AgentEnvironment::default())
                .expect("changed context")
                .expect("changed agent");
        let changed_route = agent_route_reference(&changed_agent, "m3");
        assert_eq!(first_route, changed_route);
        let error = persist_agent_route(&route_path, &changed_route, &changed_agent)
            .expect_err("explicit surface change must fail");
        assert!(error.message().contains("explicit agent surface changed"));

        persist_agent_route_with_transfer(&route_path, &changed_route, &changed_agent, true)
            .expect("explicit transfer updates the diagnosed surface");
        let transferred = load_agent_route(&route_path)
            .expect("load transferred route")
            .expect("transferred route");
        assert_eq!(transferred.agent.surface_id.as_deref(), Some("pinned-two"));
        assert_eq!(
            transferred.agent.surface_provenance,
            SurfaceProvenance::Explicit
        );
        assert_eq!(transferred.revision, 2);
    }

    #[cfg(unix)]
    #[test]
    fn intent_is_durable_before_the_first_github_mutation() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        std::fs::create_dir_all(&paths.state_dir).expect("state directory");
        std::fs::write(paths.state_dir.join("machine-tag"), "m3\n").expect("machine tag");
        let (actions, count) = handoff_status_failing_gh(&temp, &args().head);
        let mut handoff_args = explicit_agent_args("codex", "intent-owner-session");
        handoff_args.apply = true;
        let error = steward_handoff_command_without_ambient(
            &handoff_args,
            temp.path(),
            &paths,
            &actions,
            false,
            &mut Vec::new(),
        )
        .expect_err("status write should fail");
        assert!(error.message().contains("could not write handoff receipt"));
        assert_eq!(std::fs::read_to_string(count).expect("call count"), "3");

        let path = handoff_path(
            &handoff_directory(&paths, "owner/repo", handoff_args.pr),
            &handoff_args.head,
        );
        let receipt = load_handoff(&path)
            .expect("load intent")
            .expect("intent receipt");
        assert_eq!(receipt.phase, HandoffPhase::Intent);
        assert_eq!(receipt.revision, 1);
        assert_eq!(receipt.ownership_generation, 1);
        assert_eq!(receipt.origin_machine, "m3");
        assert_eq!(receipt.repair_route, RepairRoute::OriginalAgent);
    }

    #[cfg(unix)]
    #[test]
    fn same_owner_managed_replay_reconciles_without_duplicate_status() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        std::fs::create_dir_all(&paths.state_dir).expect("state directory");
        std::fs::write(paths.state_dir.join("machine-tag"), "m3\n").expect("machine tag");
        let mut handoff_args = explicit_agent_args("codex", "replay-owner-session");
        handoff_args.apply = true;
        let (actions, log) = handoff_success_gh(&temp, &handoff_args.head, "[]");

        steward_handoff_command_without_ambient(
            &handoff_args,
            temp.path(),
            &paths,
            &actions,
            false,
            &mut Vec::new(),
        )
        .expect("initial handoff");
        let replay_temp = tempfile::tempdir().expect("replay temp");
        let statuses = serde_json::json!([{
            "id": 9,
            "context": HANDOFF_CONTEXT,
            "state": "success",
            "created_at": "2026-08-27T09:00:00Z",
            "description": "Managed handoff GEN-7",
            "target_url": "https://linear.app/example/GEN-7"
        }])
        .to_string();
        let (replay_actions, replay_log) =
            handoff_success_gh(&replay_temp, &handoff_args.head, &statuses);
        steward_handoff_command_without_ambient(
            &handoff_args,
            replay_temp.path(),
            &paths,
            &replay_actions,
            false,
            &mut Vec::new(),
        )
        .expect("managed replay");

        let calls = std::fs::read_to_string(log).expect("gh log");
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.contains("repos/owner/repo/statuses/"))
                .count(),
            1
        );
        let replay_calls = std::fs::read_to_string(replay_log).expect("replay gh log");
        assert_eq!(
            replay_calls
                .lines()
                .filter(|line| line.contains("repos/owner/repo/statuses/"))
                .count(),
            0
        );
        let path = handoff_path(
            &handoff_directory(&paths, "owner/repo", handoff_args.pr),
            &handoff_args.head,
        );
        let receipt = load_handoff(&path)
            .expect("load receipt")
            .expect("managed receipt");
        assert_eq!(receipt.phase, HandoffPhase::Managed);
        assert_eq!(receipt.ownership_generation, 1);
        assert_eq!(receipt.revision, 5);
    }

    #[cfg(unix)]
    #[test]
    fn intent_replay_reconciles_an_already_accepted_status_without_reposting() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        std::fs::create_dir_all(&paths.state_dir).expect("state directory");
        std::fs::write(paths.state_dir.join("machine-tag"), "m3\n").expect("machine tag");
        let mut handoff_args = explicit_agent_args("codex", "uncertain-owner-session");
        handoff_args.apply = true;
        let origin = resolve_origin_machine(&paths).expect("machine identity");
        let agent =
            resolve_agent_context_with_environment(&handoff_args, &AgentEnvironment::default())
                .expect("agent context")
                .expect("agent");
        let route = agent_route_reference(&agent, &origin);
        let receipt =
            prepare_handoff_receipt(None, &handoff_args, "owner/repo", &origin, Some(route))
                .expect("receipt");
        let directory = handoff_directory(&paths, "owner/repo", handoff_args.pr);
        ensure_private_directory(&directory).expect("handoff directory");
        persist_handoff(
            &handoff_path(&directory, &handoff_args.head),
            receipt,
            HandoffPhase::Intent,
        )
        .expect("uncertain intent");

        let statuses = serde_json::json!([{
            "id": 7,
            "context": HANDOFF_CONTEXT,
            "state": "success",
            "created_at": "2026-08-27T07:00:00Z",
            "description": "Managed handoff GEN-7",
            "target_url": "https://linear.app/example/GEN-7"
        }])
        .to_string();
        let (actions, log) = handoff_success_gh(&temp, &handoff_args.head, &statuses);
        steward_handoff_command_without_ambient(
            &handoff_args,
            temp.path(),
            &paths,
            &actions,
            false,
            &mut Vec::new(),
        )
        .expect("reconciled handoff");

        let calls = std::fs::read_to_string(log).expect("gh log");
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.contains("repos/owner/repo/statuses/") && line.contains("POST"))
                .count(),
            0
        );
        let stored = load_handoff(&handoff_path(&directory, &handoff_args.head))
            .expect("load receipt")
            .expect("managed receipt");
        assert_eq!(stored.phase, HandoffPhase::Managed);
    }

    #[cfg(unix)]
    #[test]
    fn managed_replay_restores_success_when_latest_status_is_failure() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        std::fs::create_dir_all(&paths.state_dir).expect("state directory");
        std::fs::write(paths.state_dir.join("machine-tag"), "m3\n").expect("machine tag");
        let mut handoff_args = explicit_agent_args("codex", "status-repair-session");
        handoff_args.apply = true;
        let (actions, _) = handoff_success_gh(&temp, &handoff_args.head, "[]");
        steward_handoff_command_without_ambient(
            &handoff_args,
            temp.path(),
            &paths,
            &actions,
            false,
            &mut Vec::new(),
        )
        .expect("initial handoff");

        let replay_temp = tempfile::tempdir().expect("replay temp");
        let statuses = serde_json::json!([
            {
                "id": 7,
                "context": HANDOFF_CONTEXT,
                "state": "success",
                "created_at": "2026-08-27T07:00:00Z",
                "description": "Managed handoff GEN-7",
                "target_url": "https://linear.app/example/GEN-7"
            },
            {
                "id": 8,
                "context": HANDOFF_CONTEXT,
                "state": "failure",
                "created_at": "2026-08-27T08:00:00Z",
                "description": "revoked",
                "target_url": "https://linear.app/example/GEN-7"
            }
        ])
        .to_string();
        let (replay_actions, replay_log) =
            handoff_success_gh(&replay_temp, &handoff_args.head, &statuses);
        steward_handoff_command_without_ambient(
            &handoff_args,
            replay_temp.path(),
            &paths,
            &replay_actions,
            false,
            &mut Vec::new(),
        )
        .expect("repair current status");
        let calls = std::fs::read_to_string(replay_log).expect("replay gh log");
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.contains("repos/owner/repo/statuses/"))
                .count(),
            1
        );
    }

    #[test]
    fn latest_handoff_status_wins_regardless_of_api_order() {
        let old_success = serde_json::json!({
            "id": 7,
            "context": HANDOFF_CONTEXT,
            "state": "success",
            "created_at": "2026-08-27T07:00:00Z"
        });
        let new_failure = serde_json::json!({
            "id": 8,
            "context": HANDOFF_CONTEXT,
            "state": "failure",
            "created_at": "2026-08-27T08:00:00Z"
        });
        for statuses in [
            vec![old_success.clone(), new_failure.clone()],
            vec![new_failure.clone(), old_success.clone()],
        ] {
            let latest = latest_handoff_status(&statuses)
                .expect("freshness")
                .expect("matching status");
            assert_eq!(latest["id"], 8);
            assert_eq!(latest["state"], "failure");
        }
    }

    #[cfg(unix)]
    #[test]
    fn exact_integration_permission_error_fails_closed_without_ambient_fallback() {
        let temp = tempfile::tempdir().expect("temp");
        let (actions, count) = sequenced_gh(&temp, "Resource not accessible by integration");
        let error = run_steward_write(&actions, &["api".to_owned(), "test".to_owned()])
            .expect_err("configured App denial must fail closed");
        assert!(
            error
                .to_string()
                .contains("Resource not accessible by integration")
        );
        assert_eq!(std::fs::read_to_string(count).expect("count"), "1");
    }

    #[cfg(unix)]
    #[test]
    fn generic_write_failure_does_not_escape_to_ambient_auth() {
        let temp = tempfile::tempdir().expect("temp");
        let (actions, count) = sequenced_gh(&temp, "HTTP 403 generic forbidden");
        assert!(run_steward_write(&actions, &["api".to_owned(), "test".to_owned()]).is_err());
        assert_eq!(std::fs::read_to_string(count).expect("count"), "1");
    }

    #[cfg(unix)]
    #[test]
    fn removing_an_absent_explanatory_label_is_idempotent() {
        let temp = tempfile::tempdir().expect("temp");
        let (actions, count) = sequenced_gh(&temp, "HTTP 404 label not found");
        remove_label(&actions, "owner/repo", 7, UNMANAGED_LABEL)
            .expect("absent label is already clear");
        assert_eq!(std::fs::read_to_string(count).expect("count"), "1");
    }
}
