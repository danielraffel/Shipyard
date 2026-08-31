use super::launch_profile::{
    LaunchProfileV1, launch_profile_digest, launch_profile_integrity_hash, load_launch_profile,
    validate_launch_profile,
};
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
use std::process::{Command, ExitCode};

#[cfg(not(test))]
use crate::config::LoadedConfig;
use crate::paths::RuntimePaths;
use crate::queue::replace_file_with_windows_retry;
use crate::terminal_delivery_authority::{
    ProductionTerminalEvidenceAdapter, TerminalCapabilityRequest, TerminalEvidenceAdapter,
};
use crate::work_ledger::{
    FreshAgentLaunchProfile, NativePublicationReport, NativePublicationRequest, WorkLedger,
};
use crate::workstream_activation_loader::{
    ReadyWorkstreamActivation, WorkstreamActivationLoader, WorkstreamActivationState,
};

mod disposition;
use disposition::{AgentDisposition, StoredDispositionProofV1, load_pause_proof};

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
    pub(crate) launch_profile: Option<std::path::PathBuf>,
    pub(crate) task_graph: Option<std::path::PathBuf>,
    pub(crate) goal_managed: bool,
    pub(crate) after_handoff: String,
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
    goal_managed: bool,
    goal_lifecycle: GoalLifecycle,
    goal_status: GoalStatus,
    goal_status_provenance: GoalStatusProvenance,
    resume_transport: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct AgentRouteReference {
    route_id: String,
    owner_id: String,
    provider: String,
    origin_machine: String,
    goal_managed: bool,
    goal_lifecycle: GoalLifecycle,
    goal_status: GoalStatus,
    goal_status_provenance: GoalStatusProvenance,
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
struct StoredLaunchProfileV1 {
    generation: u64,
    revision: u64,
    profile_digest: String,
    integrity_hash: String,
    profile: LaunchProfileV1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LaunchProfileCandidateV1 {
    profile_digest: String,
    profile: LaunchProfileV1,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum GoalLifecycle {
    Unmanaged,
    Managed,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum GoalStatus {
    Unmanaged,
    Unknown,
    Active,
    Paused,
    Blocked,
    Complete,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum GoalStatusProvenance {
    // cmux lifecycle is intentionally not goal lifecycle. Until an exact
    // session's latest structured update_goal event is captured by a future
    // authority reader, downstream automation must treat goal status as
    // unknown rather than infer it from a running terminal/session.
    NotObserved,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch_profile: Option<StoredLaunchProfileV1>,
    goal_lifecycle: GoalLifecycle,
    goal_status: GoalStatus,
    goal_status_provenance: GoalStatusProvenance,
    phase: HandoffPhase,
    #[serde(default)]
    requested_agent_disposition: AgentDisposition,
    #[serde(default)]
    agent_disposition: AgentDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disposition_proof: Option<StoredDispositionProofV1>,
    #[serde(default)]
    pause_required: bool,
    wake_consumer_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_publication: Option<NativePublicationReceiptV1>,
    created_at: String,
    updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct NativePublicationReceiptV1 {
    schema_version: u32,
    state: NativePublicationStateV1,
    work_id: String,
    route_ref: String,
    wake_id: String,
    profile_digest: String,
    #[serde(default)]
    repo_policy_revision: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum NativePublicationStateV1 {
    Pending,
    Accepted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StewardHandoffTransferReport {
    pub(crate) wake_consumer_available: bool,
    pub(crate) agent_disposition: String,
    pub(crate) pause_required: bool,
    pub(crate) publication_work_id: Option<String>,
    pub(crate) publication_route_ref: Option<String>,
    pub(crate) publication_wake_id: Option<String>,
}

pub(crate) fn steward_handoff_command<W: Write>(
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
    let launch_profile = args
        .launch_profile
        .as_deref()
        .map(load_launch_profile)
        .transpose()?
        .map(|profile| prepare_launch_profile_candidate(profile, &repo, &args.head))
        .transpose()?;
    let requested_disposition = AgentDisposition::parse(&args.after_handoff)?;
    let disposition_proof = args
        .task_graph
        .as_deref()
        .map(|path| load_pause_proof(path, &args.workstream_id))
        .transpose()?;
    if !args.apply
        && requested_disposition == AgentDisposition::Pause
        && disposition_proof.is_none()
    {
        return Err(CliFailure::new(
            1,
            "--after-handoff pause requires --task-graph for a new or dry-run handoff",
        ));
    }
    let origin_machine = if args.apply {
        resolve_origin_machine(runtime_paths)?
    } else {
        preview_origin_machine(runtime_paths)?
    };
    let agent_route = agent
        .as_ref()
        .map(|agent| agent_route_reference(agent, &origin_machine));
    validate_launch_profile_route(launch_profile.as_ref(), agent_route.as_ref())?;

    let mut wake_consumer_available = false;
    let mut agent_disposition = AgentDisposition::Continue;
    let mut pause_required = false;
    if args.apply {
        let directory = handoff_directory(runtime_paths, &repo, args.pr);
        ensure_private_directory(&directory)?;
        let _handoff_lock = acquire_handoff_lock(&directory, &args.head)?;
        let path = handoff_path(&directory, &args.head);
        let route_path = agent_route
            .as_ref()
            .map(|route| agent_route_path(runtime_paths, &route.route_id));
        let mut receipt = prepare_handoff_receipt_with_profile_and_disposition(
            load_handoff(&path)?,
            args,
            &repo,
            &origin_machine,
            agent_route.clone(),
            launch_profile,
            disposition_proof,
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
        if receipt.launch_profile.is_some() {
            let ready = ready_workstream_activation(runtime_paths)?;
            receipt = publish_managed_handoff(
                runtime_paths,
                actions,
                &path,
                receipt,
                &repo,
                args.pr,
                &args.head.to_ascii_lowercase(),
                &ready,
            )?;
        }
        wake_consumer_available = receipt.wake_consumer_available;
        agent_disposition = receipt.agent_disposition;
        pause_required = receipt.pause_required;
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
        agent_disposition,
        pause_required,
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
    if crate::work_ledger::validate_workstream_handle(&args.workstream_id).is_err()
        && !is_legacy_pr_fallback(args)
    {
        return Err(CliFailure::new(
            1,
            "--workstream-id must be a canonical GEN-style handle",
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
    let disposition = AgentDisposition::parse(&args.after_handoff)?;
    if disposition == AgentDisposition::Continue && args.task_graph.is_some() {
        return Err(CliFailure::new(
            1,
            "--task-graph is accepted only with --after-handoff pause",
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
    args.launch_profile.is_none()
        && !args.goal_managed
        && args.agent_provider.is_none()
        && args.agent_session_id.is_none()
        && args.agent_parent_session_id.is_none()
        && args.agent_surface_id.is_none()
        && args.task_graph.is_none()
        && args.after_handoff == "continue"
        && !args.transfer_agent_owner
        && args.repo.as_deref().is_some_and(|repository| {
            let normalized = repository.to_ascii_lowercase();
            normalized == repository
                && normalized.split('/').count() == 2
                && args.workstream_id == format!("{normalized}#{}", args.pr)
        })
}

fn validate_resolved_workstream_identity(
    args: &StewardHandoffArgs,
    agent: Option<&AgentResumeContext>,
) -> Result<(), CliFailure> {
    if crate::work_ledger::validate_workstream_handle(&args.workstream_id).is_ok() {
        return Ok(());
    }
    if !is_legacy_pr_fallback(args) {
        return Err(CliFailure::new(
            1,
            "--workstream-id must be a canonical GEN-style handle",
        ));
    }
    if agent.is_some() {
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
    goal_managed: bool,
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
        goal_managed: env::var("SHIPYARD_GOAL_MANAGED").as_deref() == Ok("1"),
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
    let goal_managed = args.goal_managed || environment.goal_managed;
    if args.after_handoff == "pause" && !goal_managed {
        return Err(CliFailure::new(
            1,
            "--after-handoff pause requires --goal-managed",
        ));
    }
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
        if goal_managed {
            return Err(CliFailure::new(
                1,
                "--goal-managed requires a resumable agent session",
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
        goal_managed,
        goal_lifecycle: if goal_managed {
            GoalLifecycle::Managed
        } else {
            GoalLifecycle::Unmanaged
        },
        goal_status: if goal_managed {
            GoalStatus::Unknown
        } else {
            GoalStatus::Unmanaged
        },
        goal_status_provenance: GoalStatusProvenance::NotObserved,
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
                    if agent.goal_managed {
                        "goal"
                    } else {
                        "session"
                    },
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
                if agent.goal_managed {
                    "goal"
                } else {
                    "session"
                },
                &agent.resume_transport,
            ],
        ),
    };
    AgentRouteReference {
        route_id,
        owner_id,
        provider: agent.provider.clone(),
        origin_machine: origin_machine.to_owned(),
        goal_managed: agent.goal_managed,
        goal_lifecycle: agent.goal_lifecycle,
        goal_status: agent.goal_status,
        goal_status_provenance: agent.goal_status_provenance,
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

#[cfg(unix)]
#[allow(dead_code)] // Explicit manual compatibility path; active stewardship requires v2 identity.
pub(super) fn migrate_legacy_native_policy_authority(
    state_dir: &Path,
    repo: &str,
    pr: u64,
    head: &str,
) -> Result<(), String> {
    let path = state_dir
        .join("merge-steward")
        .join("handoffs")
        .join(encode_path_segment(&repo.to_ascii_lowercase()))
        .join(format!("pr-{pr}"))
        .join(format!("{}.json", head.to_ascii_lowercase()));
    let mut receipt = load_handoff(&path)
        .map_err(|error| error.message)?
        .ok_or_else(|| "legacy native handoff receipt is unavailable".to_owned())?;
    validate_handoff_receipt_integrity(&receipt, repo, pr, head).map_err(|error| error.message)?;
    let publication = receipt
        .native_publication
        .as_mut()
        .ok_or_else(|| "legacy native publication receipt is unavailable".to_owned())?;
    if !receipt.wake_consumer_available
        || publication.schema_version != 1
        || publication.state != NativePublicationStateV1::Accepted
        || publication.repo_policy_revision != 0
    {
        return Err("legacy native publication is not accepted migration authority".to_owned());
    }
    crate::work_ledger::bind_legacy_native_policy(state_dir, repo, pr, head, &publication.work_id)
        .map_err(|error| error.to_string())?;
    let ledger = WorkLedger::open_existing(state_dir)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "native work ledger is unavailable".to_owned())?;
    let policy = ledger
        .repo_policy(repo)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "explicit repository policy is unavailable".to_owned())?;
    publication.schema_version = 2;
    publication.repo_policy_revision = policy.revision;
    receipt.schema_version = 4;
    persist_handoff(&path, receipt, HandoffPhase::Managed).map_err(|error| error.message)?;
    Ok(())
}

/// Load and normalize one exact managed handoff into native ledger authority.
///
/// This reader performs no mutation. Publication policy is intentionally
/// applied later, before the ledger can create storage.
#[allow(clippy::too_many_lines)]
pub(crate) fn native_publication_request(
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    head: &str,
) -> Result<NativePublicationRequest, CliFailure> {
    if repo != repo.to_ascii_lowercase()
        || repo.trim() != repo
        || pr == 0
        || !is_full_sha(head)
        || head != head.to_ascii_lowercase()
    {
        return Err(CliFailure::new(
            1,
            "native publication requires canonical repo, PR, and lowercase exact head",
        ));
    }
    let path = handoff_path(&handoff_directory(runtime_paths, repo, pr), head);
    let trusted_actions = trusted_native_publication_actions(runtime_paths, actions, repo)?;
    let source_authority = observe_native_source_authority(&trusted_actions, repo, pr, head)?;
    let ledger = WorkLedger::open_existing(&runtime_paths.state_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .ok_or_else(|| CliFailure::new(1, "explicit repository policy is unavailable"))?;
    let repo_policy = ledger
        .repo_policy(&source_authority.canonical_repository)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .ok_or_else(|| CliFailure::new(1, "explicit repository policy is unavailable"))?;
    let receipt = load_handoff(&path)?
        .ok_or_else(|| CliFailure::new(1, "exact-head durable handoff receipt is unavailable"))?;
    validate_handoff_receipt_integrity(&receipt, repo, pr, head)?;
    if receipt.phase != HandoffPhase::Managed {
        return Err(CliFailure::new(
            1,
            "native publication requires a managed durable handoff",
        ));
    }
    let route = receipt.agent_route.as_ref().ok_or_else(|| {
        CliFailure::new(
            1,
            "native publication requires an exact private agent route",
        )
    })?;
    let private_route = load_agent_route(&agent_route_path(runtime_paths, &route.route_id))?
        .ok_or_else(|| CliFailure::new(1, "managed handoff lost its private agent route"))?;
    let recomputed_route =
        agent_route_reference(&private_route.agent, &private_route.origin_machine);
    if private_route.schema_version != 2
        || private_route.revision == 0
        || private_route.route_id != route.route_id
        || private_route.owner_id != route.owner_id
        || private_route.origin_machine != receipt.origin_machine
        || recomputed_route != *route
    {
        return Err(CliFailure::new(
            1,
            "managed handoff and private agent route identity disagree",
        ));
    }
    let stored = receipt
        .launch_profile
        .as_ref()
        .ok_or_else(|| CliFailure::new(1, "native publication requires an exact launch profile"))?;
    let profile = &stored.profile;
    let session = profile.session.as_ref().ok_or_else(|| {
        CliFailure::new(1, "native publication requires exact session provenance")
    })?;
    let bootstrap = profile.continuation_bootstrap.as_ref().ok_or_else(|| {
        CliFailure::new(
            1,
            "native publication requires continuation bootstrap authority",
        )
    })?;
    if !profile.permits_fresh_agent()
        || route.provider != session.agent_provider
        || private_route.agent.session_id != session.provider_session_id
        || receipt.workstream_id != bootstrap.workstream_handle
        || receipt.context_url != bootstrap.context_url
    {
        return Err(CliFailure::new(
            1,
            "handoff route, launch profile, and continuation authority disagree",
        ));
    }
    profile.validate_native_fresh_agent_grammar()?;
    let protected_profile_bytes = profile
        .protected_profile_bytes()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let native_resume_bytes = serde_json::to_vec(&profile.resume_argv)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let route_environment_bytes = serde_json::to_vec(&profile.route_environment)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let route_wrapper = profile
        .resume_argv
        .first()
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| CliFailure::new(1, "native resume wrapper is missing"))?;

    Ok(NativePublicationRequest {
        repository_provider: source_authority.repository_provider,
        repository_id: source_authority.repository_id,
        legacy_repository_alias: (source_authority.canonical_repository != receipt.repo)
            .then(|| receipt.repo.to_ascii_lowercase()),
        repository: source_authority.canonical_repository,
        pull_request: receipt.pr,
        head_sha: receipt.head_sha.to_ascii_lowercase(),
        base_ref: source_authority.base_ref,
        base_sha: source_authority.base_sha,
        github_installation_id: source_authority.installation_id,
        repo_policy_revision: repo_policy.revision,
        terminal_authority: private_route
            .agent
            .terminal_authority
            .or_else(test_terminal_authority)
            .ok_or_else(|| {
                CliFailure::new(1, "native publication requires live terminal authority")
            })?,
        workstream_handle: bootstrap.workstream_handle.clone(),
        plan_sha256: bootstrap.plan_sha256.clone(),
        root_revision: bootstrap.root_revision,
        issue_revision: bootstrap.issue_revision,
        projection_revision: bootstrap.projection_revision,
        material_event_revision: bootstrap.material_event_revision,
        context_url: bootstrap.context_url.clone(),
        origin_machine: receipt.origin_machine.clone(),
        owner_id: receipt.owner_id.clone(),
        owner_generation: receipt.ownership_generation,
        agent_provider: route.provider.clone(),
        agent_session_id: private_route.agent.session_id,
        route_account: profile
            .provider
            .account
            .clone()
            .unwrap_or_else(|| "unselected-account".into()),
        route_model: profile
            .provider
            .model
            .clone()
            .unwrap_or_else(|| "unselected-model".into()),
        route_wrapper,
        native_resume_digest: hex::encode(Sha256::digest(native_resume_bytes)),
        route_environment_digest: hex::encode(Sha256::digest(route_environment_bytes)),
        route_id: route.route_id.clone(),
        profile_generation: stored.generation,
        profile_revision: stored.revision,
        profile_provider: profile.provider.provider.clone(),
        profile_digest: stored.profile_digest.clone(),
        protected_profile_bytes,
        success_continuation_digest: bootstrap.success_continuation_digest.clone(),
        failure_continuation_digest: bootstrap.failure_continuation_digest.clone(),
    })
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
fn trusted_native_publication_actions(
    _: &RuntimePaths,
    actions: &GitHubActions,
    repo: &str,
) -> Result<GitHubActions, CliFailure> {
    Ok(actions.clone().with_repo_override(repo))
}

#[cfg(not(test))]
fn trusted_native_publication_actions(
    runtime_paths: &RuntimePaths,
    _: &GitHubActions,
    repo: &str,
) -> Result<GitHubActions, CliFailure> {
    let config = LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let cwd = std::env::current_dir()
        .map_err(|error| CliFailure::new(1, format!("resolve native authority cwd: {error}")))?;
    Ok(GitHubActions::from_loaded_config(cwd, &config).with_repo_override(repo))
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
fn test_terminal_authority() -> Option<TerminalCapabilityRequest> {
    Some(TerminalCapabilityRequest::Cmux {
        cli_path: "/test/cmux".into(),
        socket_path: "/test/cmux.sock".into(),
        surface_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
        workspace_id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into(),
        native_session_id: "provider-session-7".into(),
        provider_kind: "codex".into(),
        process: crate::terminal_delivery_authority::LocalProcessIncarnation {
            boot_id: "test-boot".into(),
            pid: 42,
            start_identity: "test-start".into(),
        },
    })
}

#[cfg(not(test))]
fn test_terminal_authority() -> Option<TerminalCapabilityRequest> {
    None
}

struct NativeSourceAuthority {
    installation_id: u64,
    repository_provider: String,
    repository_id: String,
    canonical_repository: String,
    base_ref: String,
    base_sha: String,
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn verify_native_repository_identity(
    actions: &GitHubActions,
    repository_provider: &str,
    repository_id: &str,
    repository: &str,
) -> Result<(), String> {
    if repository_provider != "github.com" {
        return Err("native steward repository provider is unsupported".to_owned());
    }
    let observed = gh_json(
        actions,
        &[
            "repo".into(),
            "view".into(),
            repository.to_owned(),
            "--json".into(),
            "id,nameWithOwner".into(),
        ],
        "authenticate native steward repository identity",
    )?;
    let observed_id = observed.get("id").and_then(Value::as_str);
    let observed_repository = observed
        .get("nameWithOwner")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    if observed_id != Some(repository_id) || observed_repository.as_deref() != Some(repository) {
        return Err("native steward repository identity changed".to_owned());
    }
    Ok(())
}

fn observe_native_source_authority(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    head: &str,
) -> Result<NativeSourceAuthority, CliFailure> {
    let installation_id = actions
        .app_installation_id()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let repository = gh_json(
        actions,
        &[
            "repo".into(),
            "view".into(),
            repo.to_owned(),
            "--json".into(),
            "id,nameWithOwner".into(),
        ],
        "observe immutable native publication repository identity",
    )
    .map_err(|error| CliFailure::new(1, error))?;
    let repository_id = repository
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .ok_or_else(|| CliFailure::new(1, "native publication repository ID is unavailable"))?;
    let canonical_coordinate = repository
        .get("nameWithOwner")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let canonical_repository = canonical_coordinate.ok_or_else(|| {
        CliFailure::new(1, "native publication canonical repository is unavailable")
    })?;
    let snapshot = gh_json(
        actions,
        &[
            "pr".into(),
            "view".into(),
            pr.to_string(),
            "--repo".into(),
            repo.to_owned(),
            "--json".into(),
            "state,headRefOid,baseRefName,baseRefOid".into(),
        ],
        "observe exact native publication source",
    )
    .map_err(|error| CliFailure::new(1, error))?;
    let pull_request = &snapshot;
    let observed_head = pull_request.get("headRefOid").and_then(Value::as_str);
    let base_ref = pull_request
        .get("baseRefName")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let base_sha = pull_request
        .get("baseRefOid")
        .and_then(Value::as_str)
        .filter(|value| is_full_sha(value));
    if pull_request.get("state").and_then(Value::as_str) != Some("OPEN")
        || observed_head != Some(head)
        || base_ref.is_none()
        || base_sha.is_none()
    {
        return Err(CliFailure::new(
            1,
            "native publication source head/base authority changed",
        ));
    }
    Ok(NativeSourceAuthority {
        installation_id,
        repository_provider: "github.com".to_owned(),
        repository_id: repository_id.to_owned(),
        canonical_repository,
        base_ref: base_ref.expect("checked").to_owned(),
        base_sha: base_sha.expect("checked").to_ascii_lowercase(),
    })
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
        agent_disposition: receipt.agent_disposition.as_str().to_owned(),
        pause_required: receipt.pause_required,
        publication_work_id: receipt
            .native_publication
            .as_ref()
            .map(|publication| publication.work_id.clone()),
        publication_route_ref: receipt
            .native_publication
            .as_ref()
            .map(|publication| publication.route_ref.clone()),
        publication_wake_id: receipt
            .native_publication
            .as_ref()
            .map(|publication| publication.wake_id.clone()),
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
    pub(super) provider_route: Option<ProviderRouteReferenceV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ProviderRouteReferenceV1 {
    pub(super) profile_digest: String,
    pub(super) integrity_hash: String,
    pub(super) generation: u64,
    pub(super) revision: u64,
    pub(super) provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
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
    let provider_route = provider_route_reference(&receipt);
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
        provider_route,
    }))
}

pub(super) fn terminal_owner_route_or_unresolved(
    state_dir: &Path,
    repo: &str,
    pr: u64,
    head: &str,
) -> Option<TerminalOwnerRoute> {
    // Route transport is not deployed authority. Corrupt, missing, or stale
    // private state therefore remains an unroutable ledger obligation instead
    // of blocking deterministic stewardship or authorizing a fresh agent.
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
    let provider_route = provider_route_reference(&receipt);
    Some(TerminalOwnerRoute {
        origin_machine: receipt.origin_machine,
        owner_id: receipt.owner_id,
        ownership_generation: receipt.ownership_generation,
        owner_disposition: "unroutable_private_route".to_owned(),
        route_id: None,
        provider: None,
        resume_transport: None,
        terminal_provenance: None,
        provider_route,
    })
}

fn provider_route_reference(receipt: &DurableStewardHandoff) -> Option<ProviderRouteReferenceV1> {
    receipt
        .launch_profile
        .as_ref()
        .map(|stored| ProviderRouteReferenceV1 {
            profile_digest: stored.profile_digest.clone(),
            integrity_hash: stored.integrity_hash.clone(),
            generation: stored.generation,
            revision: stored.revision,
            provider: stored.profile.provider.provider.clone(),
            account: stored.profile.provider.account.clone(),
            model: stored.profile.provider.model.clone(),
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

#[cfg(test)]
fn prepare_handoff_receipt(
    existing: Option<DurableStewardHandoff>,
    args: &StewardHandoffArgs,
    repo: &str,
    origin_machine: &str,
    agent_route: Option<AgentRouteReference>,
) -> Result<DurableStewardHandoff, CliFailure> {
    prepare_handoff_receipt_with_profile(existing, args, repo, origin_machine, agent_route, None)
}

#[cfg(test)]
fn prepare_handoff_receipt_with_profile(
    existing: Option<DurableStewardHandoff>,
    args: &StewardHandoffArgs,
    repo: &str,
    origin_machine: &str,
    agent_route: Option<AgentRouteReference>,
    launch_profile: Option<LaunchProfileCandidateV1>,
) -> Result<DurableStewardHandoff, CliFailure> {
    prepare_handoff_receipt_with_profile_and_disposition(
        existing,
        args,
        repo,
        origin_machine,
        agent_route,
        launch_profile,
        None,
    )
}

#[allow(clippy::too_many_lines)]
fn prepare_handoff_receipt_with_profile_and_disposition(
    existing: Option<DurableStewardHandoff>,
    args: &StewardHandoffArgs,
    repo: &str,
    origin_machine: &str,
    agent_route: Option<AgentRouteReference>,
    launch_profile: Option<LaunchProfileCandidateV1>,
    disposition_proof: Option<StoredDispositionProofV1>,
) -> Result<DurableStewardHandoff, CliFailure> {
    let normalized_repo = repo.to_ascii_lowercase();
    let normalized_head = args.head.to_ascii_lowercase();
    let requested_disposition = AgentDisposition::parse(&args.after_handoff)?;
    let owner_id = agent_route.as_ref().map_or_else(
        || "fresh-agent-only".to_owned(),
        |route| route.owner_id.clone(),
    );
    let goal_lifecycle = agent_route
        .as_ref()
        .map_or(GoalLifecycle::Unmanaged, |route| route.goal_lifecycle);
    let goal_status = agent_route
        .as_ref()
        .map_or(GoalStatus::Unmanaged, |route| route.goal_status);
    let goal_status_provenance = agent_route
        .as_ref()
        .map_or(GoalStatusProvenance::NotObserved, |route| {
            route.goal_status_provenance
        });
    validate_launch_profile_route(launch_profile.as_ref(), agent_route.as_ref())?;
    if let Some(existing) = existing {
        validate_existing_handoff(&existing, args, &normalized_repo, &normalized_head)?;
        let disposition_proof = match requested_disposition {
            AgentDisposition::Continue => None,
            AgentDisposition::Pause => Some(
                disposition_proof
                    .or_else(|| existing.disposition_proof.clone())
                    .filter(|proof| proof.valid_for(&args.workstream_id))
                    .ok_or_else(|| {
                        CliFailure::new(
                            1,
                            "--after-handoff pause requires a valid durable task-graph proof",
                        )
                    })?,
            ),
        };
        if args.transfer_agent_owner {
            return transfer_handoff_owner(
                existing,
                args,
                origin_machine,
                owner_id,
                agent_route,
                goal_lifecycle,
                goal_status,
                goal_status_provenance,
                disposition_proof,
                launch_profile,
            );
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
        if !same_launch_profile_replay(existing.launch_profile.as_ref(), launch_profile.as_ref()) {
            return Err(CliFailure::new(
                1,
                "same-owner handoff cannot change or omit its launch profile",
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
        if existing.requested_agent_disposition != requested_disposition
            || existing.disposition_proof != disposition_proof
        {
            return Err(CliFailure::new(
                1,
                "same-owner handoff cannot change agent disposition or task-graph proof",
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
    let disposition_proof = match requested_disposition {
        AgentDisposition::Continue => None,
        AgentDisposition::Pause => Some(disposition_proof.ok_or_else(|| {
            CliFailure::new(
                1,
                "--after-handoff pause requires --task-graph proving no independent runnable work",
            )
        })?),
    };
    Ok(new_handoff_receipt(
        args,
        normalized_repo,
        normalized_head,
        origin_machine,
        owner_id,
        agent_route,
        goal_lifecycle,
        goal_status,
        goal_status_provenance,
        requested_disposition,
        disposition_proof,
        launch_profile,
    ))
}

fn validate_launch_profile_route(
    profile: Option<&LaunchProfileCandidateV1>,
    route: Option<&AgentRouteReference>,
) -> Result<(), CliFailure> {
    let Some(profile) = profile else {
        return Ok(());
    };
    if let Some(route) = route {
        let Some(session) = profile.profile.session.as_ref() else {
            return Err(CliFailure::new(
                1,
                "an exact-session launch profile requires provider-session provenance",
            ));
        };
        if opaque_id(
            "owner",
            &[&session.agent_provider, &session.provider_session_id],
        ) != route.owner_id
        {
            return Err(CliFailure::new(
                1,
                "launch profile provider-session provenance does not match the durable agent route",
            ));
        }
        return Ok(());
    }
    if profile.profile.recovery_policy
        != super::launch_profile::RecoveryPolicyV1::FreshCheckpointOnly
    {
        return Err(CliFailure::new(
            1,
            "an exact-session launch profile requires a durable agent route",
        ));
    }
    if profile.profile.session.is_some() {
        return Err(CliFailure::new(
            1,
            "a fresh-checkpoint-only profile cannot claim existing provider-session provenance",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn new_handoff_receipt(
    args: &StewardHandoffArgs,
    normalized_repo: String,
    normalized_head: String,
    origin_machine: &str,
    owner_id: String,
    agent_route: Option<AgentRouteReference>,
    goal_lifecycle: GoalLifecycle,
    goal_status: GoalStatus,
    goal_status_provenance: GoalStatusProvenance,
    requested_disposition: AgentDisposition,
    disposition_proof: Option<StoredDispositionProofV1>,
    launch_profile: Option<LaunchProfileCandidateV1>,
) -> DurableStewardHandoff {
    let now = Utc::now().to_rfc3339();
    let launch_profile =
        launch_profile.map(|profile| bind_launch_profile(profile, agent_route.as_ref(), 1, 1));
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
        launch_profile,
        goal_lifecycle,
        goal_status,
        goal_status_provenance,
        requested_agent_disposition: requested_disposition,
        agent_disposition: AgentDisposition::Continue,
        pause_required: false,
        disposition_proof,
        wake_consumer_available: false,
        native_publication: None,
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
                && receipt.goal_lifecycle == GoalLifecycle::Unmanaged
                && receipt.goal_status == GoalStatus::Unmanaged
                && receipt.goal_status_provenance == GoalStatusProvenance::NotObserved
        },
        |route| {
            receipt.repair_route == RepairRoute::OriginalAgent
                && receipt.owner_id == route.owner_id
                && receipt.origin_machine == route.origin_machine
                && receipt.goal_lifecycle == route.goal_lifecycle
                && receipt.goal_status == route.goal_status
                && receipt.goal_status_provenance == route.goal_status_provenance
        },
    );
    let disposition_consistent = match receipt.schema_version {
        2 => {
            receipt.requested_agent_disposition == AgentDisposition::Continue
                && receipt.agent_disposition == AgentDisposition::Continue
                && !receipt.pause_required
                && receipt.disposition_proof.is_none()
        }
        3 | 4 => match receipt.requested_agent_disposition {
            AgentDisposition::Continue => {
                receipt.agent_disposition == AgentDisposition::Continue
                    && !receipt.pause_required
                    && receipt.disposition_proof.is_none()
            }
            AgentDisposition::Pause => {
                receipt
                    .disposition_proof
                    .as_ref()
                    .is_some_and(|proof| proof.valid_for(&receipt.workstream_id))
                    && receipt
                        .agent_route
                        .as_ref()
                        .is_some_and(|route| route.goal_managed)
                    && if receipt.wake_consumer_available {
                        receipt.agent_disposition == AgentDisposition::Pause
                            && receipt.pause_required
                    } else {
                        receipt.agent_disposition == AgentDisposition::Continue
                            && !receipt.pause_required
                    }
            }
        },
        _ => false,
    };
    let launch_profile_consistent = receipt.launch_profile.as_ref().is_none_or(|stored| {
        stored.generation > 0
            && stored.revision > 0
            && validate_launch_profile(&stored.profile).is_ok()
            && launch_profile_digest(&stored.profile)
                .is_ok_and(|digest| digest == stored.profile_digest)
            && launch_profile_integrity_hash(
                &stored.profile_digest,
                stored.generation,
                stored.revision,
                receipt
                    .agent_route
                    .as_ref()
                    .map(|route| route.route_id.as_str()),
            ) == stored.integrity_hash
            && stored.generation == receipt.ownership_generation
            && launch_profile_session_matches_route(&stored.profile, receipt.agent_route.as_ref())
            && stored
                .profile
                .worktree
                .repository
                .eq_ignore_ascii_case(repo)
            && stored.profile.worktree.head_sha.eq_ignore_ascii_case(head)
            && (receipt.agent_route.is_some()
                || stored.profile.recovery_policy
                    == super::launch_profile::RecoveryPolicyV1::FreshCheckpointOnly)
    });
    let publication_consistent = match (
        receipt.wake_consumer_available,
        receipt.native_publication.as_ref(),
        receipt.launch_profile.as_ref(),
    ) {
        (false, None, _) => true,
        (available, Some(publication), Some(profile)) => {
            ((publication.schema_version == 2 && publication.repo_policy_revision > 0)
                || (matches!(receipt.schema_version, 2 | 3)
                    && publication.schema_version == 1
                    && publication.repo_policy_revision == 0))
                && publication.profile_digest == profile.profile_digest
                && valid_publication_identifier(&publication.work_id)
                && valid_publication_identifier(&publication.route_ref)
                && valid_publication_identifier(&publication.wake_id)
                && matches!(
                    (available, publication.state),
                    (false, NativePublicationStateV1::Pending)
                        | (true, NativePublicationStateV1::Accepted)
                )
        }
        _ => false,
    };
    if !matches!(receipt.schema_version, 2..=4)
        || !receipt.repo.eq_ignore_ascii_case(repo)
        || receipt.pr != pr
        || !receipt.head_sha.eq_ignore_ascii_case(head)
        || receipt.ownership_generation == 0
        || receipt.revision == 0
        || !route_consistent
        || !disposition_consistent
        || !launch_profile_consistent
        || !publication_consistent
    {
        return Err(CliFailure::new(
            1,
            "durable handoff receipt is incompatible or does not match its exact-head path",
        ));
    }
    validate_agent_identifier("origin machine", &receipt.origin_machine)?;
    Ok(())
}

fn valid_publication_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn launch_profile_session_matches_route(
    profile: &LaunchProfileV1,
    route: Option<&AgentRouteReference>,
) -> bool {
    match (profile.session.as_ref(), route) {
        (Some(session), Some(route)) => {
            opaque_id(
                "owner",
                &[&session.agent_provider, &session.provider_session_id],
            ) == route.owner_id
        }
        (None, None) => {
            profile.recovery_policy == super::launch_profile::RecoveryPolicyV1::FreshCheckpointOnly
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn transfer_handoff_owner(
    mut existing: DurableStewardHandoff,
    args: &StewardHandoffArgs,
    origin_machine: &str,
    owner_id: String,
    agent_route: Option<AgentRouteReference>,
    goal_lifecycle: GoalLifecycle,
    goal_status: GoalStatus,
    goal_status_provenance: GoalStatusProvenance,
    disposition_proof: Option<StoredDispositionProofV1>,
    launch_profile: Option<LaunchProfileCandidateV1>,
) -> Result<DurableStewardHandoff, CliFailure> {
    if agent_route.is_none() {
        return Err(CliFailure::new(
            1,
            "--transfer-agent-owner requires an explicit replacement agent route",
        ));
    }
    if existing.workstream_id != args.workstream_id
        || existing.context_url != args.context_url
        || existing.requested_agent_disposition != AgentDisposition::parse(&args.after_handoff)?
        || existing.disposition_proof != disposition_proof
    {
        return Err(CliFailure::new(
            1,
            "ownership transfer cannot change workstream, context, or disposition",
        ));
    }
    if existing.owner_id == owner_id
        && existing.agent_route == agent_route
        && same_launch_profile_replay(existing.launch_profile.as_ref(), launch_profile.as_ref())
    {
        return Ok(existing);
    }
    if existing.native_publication.is_some() {
        return Err(CliFailure::new(
            1,
            "published native continuation ownership cannot be transferred; create a new exact-head handoff",
        ));
    }
    if existing.owner_id == owner_id && existing.agent_route == agent_route {
        return Err(CliFailure::new(
            1,
            "launch profile replacement requires a replacement agent owner",
        ));
    }
    existing.owner_id = owner_id;
    existing.agent_route = agent_route;
    origin_machine.clone_into(&mut existing.origin_machine);
    existing.repair_route = RepairRoute::OriginalAgent;
    existing.goal_lifecycle = goal_lifecycle;
    existing.goal_status = goal_status;
    existing.goal_status_provenance = goal_status_provenance;
    existing.disposition_proof = disposition_proof;
    let next_generation = existing
        .ownership_generation
        .checked_add(1)
        .ok_or_else(|| CliFailure::new(1, "handoff ownership generation overflow"))?;
    existing.launch_profile = match (existing.launch_profile.as_ref(), launch_profile) {
        (Some(previous), Some(profile)) => Some(bind_launch_profile(
            profile,
            existing.agent_route.as_ref(),
            next_generation,
            previous
                .revision
                .checked_add(1)
                .ok_or_else(|| CliFailure::new(1, "launch-profile revision overflow"))?,
        )),
        (None, Some(profile)) => Some(bind_launch_profile(
            profile,
            existing.agent_route.as_ref(),
            next_generation,
            1,
        )),
        (None, None) => None,
        (Some(_), None) => {
            return Err(CliFailure::new(
                1,
                "ownership transfer cannot omit an existing launch profile",
            ));
        }
    };
    existing.ownership_generation = next_generation;
    Ok(existing)
}

fn ready_workstream_activation(
    runtime_paths: &RuntimePaths,
) -> Result<ReadyWorkstreamActivation, CliFailure> {
    let production_paths = RuntimePaths::current(crate::identity::RuntimeMode::Shipyard);
    if runtime_paths != &production_paths {
        return Err(CliFailure::new(
            1,
            "automatic native continuation publication requires canonical production roots",
        ));
    }
    let mut loader = WorkstreamActivationLoader::production();
    match loader.revalidate_for_tick() {
        WorkstreamActivationState::Ready(ready) => Ok(ready),
        WorkstreamActivationState::Disabled => Err(CliFailure::new(
            1,
            "workstream continuation activation is disabled; monitoring ownership was not transferred",
        )),
        WorkstreamActivationState::Refused(reason) => Err(CliFailure::new(
            1,
            format!(
                "workstream continuation activation refused: {}; monitoring ownership was not transferred",
                reason.code()
            ),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_managed_handoff(
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    path: &Path,
    receipt: DurableStewardHandoff,
    repo: &str,
    pr: u64,
    head: &str,
    ready: &ReadyWorkstreamActivation,
) -> Result<DurableStewardHandoff, CliFailure> {
    publish_managed_handoff_with_consumer(
        runtime_paths,
        actions,
        path,
        receipt,
        repo,
        pr,
        head,
        ready,
        |_paths, _report| Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
fn publish_managed_handoff_with_consumer<F>(
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    path: &Path,
    receipt: DurableStewardHandoff,
    repo: &str,
    pr: u64,
    head: &str,
    ready: &ReadyWorkstreamActivation,
    _await_consumer: F,
) -> Result<DurableStewardHandoff, CliFailure>
where
    F: FnOnce(&RuntimePaths, &NativePublicationReport) -> Result<(), CliFailure>,
{
    let request = native_publication_request(runtime_paths, actions, repo, pr, head)?;
    if ready.machine_tag != request.origin_machine {
        return Err(CliFailure::new(
            1,
            "durable handoff belongs to a different continuation consumer machine",
        ));
    }
    let planned = WorkLedger::plan_or_apply_native_continuation(
        &runtime_paths.state_dir,
        &request,
        &ready.config,
        false,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let receipt = bind_native_publication_pending(path, receipt, &planned)?;
    let report = WorkLedger::plan_or_apply_native_continuation(
        &runtime_paths.state_dir,
        &request,
        &ready.config,
        true,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if report.work_id != planned.work_id
        || report.route_ref != planned.route_ref
        || report.wake_id != planned.wake_id
        || report.profile_digest != planned.profile_digest
        || report.repo_policy_revision != planned.repo_policy_revision
    {
        return Err(CliFailure::new(
            1,
            "native continuation publication changed after durable intent",
        ));
    }
    // Managed publication remains wake-free. Its canonical ledger record is
    // nevertheless a durable daemon obligation, so successful exact replay is
    // the monitoring-transfer boundary; provider delivery is deliberately not
    // part of the post-handoff disposition decision.
    crate::work_ledger::verify_native_policy_binding(&runtime_paths.state_dir, repo, pr, head)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    bind_native_publication_accepted(path, receipt, &report)
}

fn native_publication_receipt(
    report: &NativePublicationReport,
    state: NativePublicationStateV1,
) -> NativePublicationReceiptV1 {
    NativePublicationReceiptV1 {
        schema_version: 2,
        state,
        work_id: report.work_id.clone(),
        route_ref: report.route_ref.clone(),
        wake_id: report.wake_id.clone(),
        profile_digest: report.profile_digest.clone(),
        repo_policy_revision: report.repo_policy_revision,
    }
}

fn bind_native_publication_pending(
    path: &Path,
    mut receipt: DurableStewardHandoff,
    report: &NativePublicationReport,
) -> Result<DurableStewardHandoff, CliFailure> {
    let pending = native_publication_receipt(report, NativePublicationStateV1::Pending);
    let accepted = native_publication_receipt(report, NativePublicationStateV1::Accepted);
    if receipt.wake_consumer_available {
        if receipt.native_publication.as_ref() == Some(&accepted) {
            return Ok(receipt);
        }
        if receipt
            .native_publication
            .as_ref()
            .is_some_and(|publication| {
                publication.schema_version == 1
                    && publication.state == NativePublicationStateV1::Accepted
                    && publication.work_id == accepted.work_id
                    && publication.route_ref == accepted.route_ref
                    && publication.wake_id == accepted.wake_id
                    && publication.profile_digest == accepted.profile_digest
            })
        {
            receipt.schema_version = 4;
            receipt.native_publication = Some(accepted);
            return persist_handoff(path, receipt, HandoffPhase::Managed);
        }
        return Err(CliFailure::new(
            1,
            "accepted native publication cannot return to pending",
        ));
    }
    match receipt.native_publication.as_ref() {
        Some(existing) if existing == &pending => Ok(receipt),
        Some(existing)
            if existing.schema_version == 1
                && existing.state == NativePublicationStateV1::Pending
                && existing.work_id == pending.work_id
                && existing.route_ref == pending.route_ref
                && existing.wake_id == pending.wake_id
                && existing.profile_digest == pending.profile_digest =>
        {
            receipt.schema_version = 4;
            receipt.native_publication = Some(pending);
            persist_handoff(path, receipt, HandoffPhase::Managed)
        }
        Some(_) => Err(CliFailure::new(
            1,
            "native publication intent changed for an existing exact-head handoff",
        )),
        None => {
            receipt.native_publication = Some(pending);
            persist_handoff(path, receipt, HandoffPhase::Managed)
        }
    }
}

fn bind_native_publication_accepted(
    path: &Path,
    mut receipt: DurableStewardHandoff,
    report: &NativePublicationReport,
) -> Result<DurableStewardHandoff, CliFailure> {
    let pending = native_publication_receipt(report, NativePublicationStateV1::Pending);
    let accepted = native_publication_receipt(report, NativePublicationStateV1::Accepted);
    if receipt.wake_consumer_available {
        if receipt.native_publication.as_ref() == Some(&accepted) {
            return Ok(receipt);
        }
        return Err(CliFailure::new(
            1,
            "accepted monitoring transfer changed its native publication",
        ));
    }
    if receipt.native_publication.as_ref() != Some(&pending) {
        return Err(CliFailure::new(
            1,
            "native publication was not durably pending before monitoring transfer",
        ));
    }
    receipt.native_publication = Some(accepted);
    receipt.wake_consumer_available = true;
    match receipt.requested_agent_disposition {
        AgentDisposition::Continue => {
            receipt.agent_disposition = AgentDisposition::Continue;
            receipt.pause_required = false;
        }
        AgentDisposition::Pause => {
            if receipt
                .disposition_proof
                .as_ref()
                .is_none_or(|proof| !proof.valid_for(&receipt.workstream_id))
                || receipt
                    .agent_route
                    .as_ref()
                    .is_none_or(|route| !route.goal_managed)
            {
                return Err(CliFailure::new(
                    1,
                    "pause disposition lost its managed-goal task-graph authority",
                ));
            }
            receipt.agent_disposition = AgentDisposition::Pause;
            receipt.pause_required = true;
        }
    }
    persist_handoff(path, receipt, HandoffPhase::Managed)
}

fn prepare_launch_profile_candidate(
    profile: LaunchProfileV1,
    repo: &str,
    head: &str,
) -> Result<LaunchProfileCandidateV1, CliFailure> {
    if !profile.worktree.repository.eq_ignore_ascii_case(repo)
        || !profile.worktree.head_sha.eq_ignore_ascii_case(head)
    {
        return Err(CliFailure::new(
            1,
            "launch profile worktree provenance must match the exact handoff repository and head",
        ));
    }
    verify_launch_profile_worktree(&profile)?;
    Ok(LaunchProfileCandidateV1 {
        profile_digest: launch_profile_digest(&profile)?,
        profile,
    })
}

fn verify_launch_profile_worktree(profile: &LaunchProfileV1) -> Result<(), CliFailure> {
    let claimed_path = Path::new(&profile.worktree.path);
    let canonical_path = claimed_path.canonicalize().map_err(|error| {
        CliFailure::new(
            1,
            format!("launch profile worktree path is unavailable: {error}"),
        )
    })?;

    let top_level = git_worktree_value(&canonical_path, &["rev-parse", "--show-toplevel"])?;
    let canonical_top_level = Path::new(&top_level).canonicalize().map_err(|error| {
        CliFailure::new(
            1,
            format!("launch profile Git top-level path is unavailable: {error}"),
        )
    })?;
    if canonical_top_level != canonical_path {
        return Err(CliFailure::new(
            1,
            "launch profile path must name the exact Git worktree root",
        ));
    }

    let observed_head = git_worktree_value(&canonical_path, &["rev-parse", "HEAD"])?;
    if !observed_head.eq_ignore_ascii_case(&profile.worktree.head_sha) {
        return Err(CliFailure::new(
            1,
            "launch profile worktree HEAD does not match its claimed exact head",
        ));
    }
    let remote = git_worktree_value(&canonical_path, &["remote", "get-url", "origin"])?;
    let observed_repo = crate::gh::parse_github_remote_slug(&remote).ok_or_else(|| {
        CliFailure::new(
            1,
            "launch profile worktree origin is not a canonical GitHub repository",
        )
    })?;
    if !observed_repo.eq_ignore_ascii_case(&profile.worktree.repository) {
        return Err(CliFailure::new(
            1,
            "launch profile worktree origin does not match its claimed repository",
        ));
    }

    let branch = git_worktree_value(
        &canonical_path,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )?;
    if profile.worktree.lineage_id != branch {
        return Err(CliFailure::new(
            1,
            "launch profile lineage ID must match the worktree's exact branch",
        ));
    }
    let lineage_key = format!("branch.{branch}.pulpWorktree");
    let status = git_worktree_value(
        &canonical_path,
        &[
            "config",
            "--local",
            "--get",
            &format!("{lineage_key}Status"),
        ],
    )?;
    let durable_head = git_worktree_value(
        &canonical_path,
        &[
            "config",
            "--local",
            "--get",
            &format!("{lineage_key}DurableSha"),
        ],
    )?;
    let last_path = git_worktree_value(
        &canonical_path,
        &[
            "config",
            "--local",
            "--get",
            &format!("{lineage_key}LastPath"),
        ],
    )?;
    let canonical_last_path = Path::new(&last_path).canonicalize().map_err(|error| {
        CliFailure::new(
            1,
            format!("launch profile lineage path is unavailable: {error}"),
        )
    })?;
    if status != "active"
        || !durable_head.eq_ignore_ascii_case(&observed_head)
        || canonical_last_path != canonical_path
    {
        return Err(CliFailure::new(
            1,
            "launch profile worktree lineage is not active at the exact path and head",
        ));
    }
    Ok(())
}

fn git_worktree_value(path: &Path, args: &[&str]) -> Result<String, CliFailure> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to inspect launch profile worktree: {error}"),
            )
        })?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            "launch profile worktree or lineage authority could not be verified",
        ));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| CliFailure::new(1, "launch profile Git metadata was not UTF-8"))
}

fn bind_launch_profile(
    candidate: LaunchProfileCandidateV1,
    route: Option<&AgentRouteReference>,
    generation: u64,
    revision: u64,
) -> StoredLaunchProfileV1 {
    StoredLaunchProfileV1 {
        integrity_hash: launch_profile_integrity_hash(
            &candidate.profile_digest,
            generation,
            revision,
            route.map(|route| route.route_id.as_str()),
        ),
        generation,
        revision,
        profile_digest: candidate.profile_digest,
        profile: candidate.profile,
    }
}

fn same_launch_profile(
    existing: Option<&StoredLaunchProfileV1>,
    incoming: Option<&LaunchProfileCandidateV1>,
) -> bool {
    match (existing, incoming) {
        (None, None) => true,
        (Some(existing), Some(incoming)) => {
            existing.profile_digest == incoming.profile_digest
                && existing.profile == incoming.profile
        }
        _ => false,
    }
}

fn same_launch_profile_replay(
    existing: Option<&StoredLaunchProfileV1>,
    incoming: Option<&LaunchProfileCandidateV1>,
) -> bool {
    incoming.is_none() && existing.is_some() || same_launch_profile(existing, incoming)
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
        && existing.goal_managed == incoming.goal_managed
        && existing.goal_lifecycle == incoming.goal_lifecycle
        && existing.goal_status == incoming.goal_status
        && existing.goal_status_provenance == incoming.goal_status_provenance
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

#[allow(clippy::too_many_arguments)]
fn render<W: Write>(
    args: &StewardHandoffArgs,
    repo: &str,
    agent_route: Option<&AgentRouteReference>,
    origin_machine: &str,
    json_output: bool,
    wake_consumer_available: bool,
    agent_disposition: AgentDisposition,
    pause_required: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json_output {
        let data = render_json_data(
            args,
            repo,
            agent_route,
            origin_machine,
            wake_consumer_available,
            agent_disposition,
            pause_required,
        )?;
        return write_json_envelope(stdout, "runner.steward-handoff", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "steward handoff: mode={} repo={} pr=#{} head={} workstream={} label={} requested_disposition={} disposition={} disposition_supported=true pause_supported=true pause_required={} monitoring_transferred={} wake_consumer_available={} origin_machine={} repair_route={}",
        if args.apply { "apply" } else { "dry-run" },
        repo,
        args.pr,
        args.head,
        args.workstream_id,
        MANAGED_LABEL,
        args.after_handoff,
        agent_disposition.as_str(),
        pause_required,
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

#[allow(clippy::too_many_arguments)]
fn render_json_data(
    args: &StewardHandoffArgs,
    repo: &str,
    agent_route: Option<&AgentRouteReference>,
    origin_machine: &str,
    wake_consumer_available: bool,
    agent_disposition: AgentDisposition,
    pause_required: bool,
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
            "agent_disposition".to_owned(),
            Value::from(agent_disposition.as_str()),
        ),
        (
            "requested_agent_disposition".to_owned(),
            Value::from(args.after_handoff.clone()),
        ),
        ("agent_disposition_supported".to_owned(), Value::from(true)),
        ("pause_required".to_owned(), Value::from(pause_required)),
        ("pause_supported".to_owned(), Value::from(true)),
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
        (
            "goal_lifecycle".to_owned(),
            Value::from(if agent_route.is_some_and(|route| route.goal_managed) {
                "managed"
            } else {
                "unmanaged"
            }),
        ),
        (
            "goal_status_provenance".to_owned(),
            Value::from("not_observed"),
        ),
    ]);
    data.insert(
        "goal_status".to_owned(),
        serde_json::to_value(agent_route.map_or(GoalStatus::Unmanaged, |route| route.goal_status))
            .map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
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
            launch_profile: None,
            task_graph: None,
            goal_managed: false,
            after_handoff: "continue".to_owned(),
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

    #[test]
    fn workstream_identifier_requires_the_canonical_gen_style_grammar() {
        for value in ["GEN 7", "gen-7", "GEN-07", "GEN-7\nspoof", "GEN-7-extra"] {
            let mut invalid = args();
            invalid.workstream_id = value.to_owned();
            assert!(validate_args(&invalid).is_err(), "accepted {value:?}");
        }
        assert!(validate_args(&args()).is_ok());
    }

    #[test]
    fn legacy_pr_fallback_is_exact_and_cannot_authorize_workstream_custody() {
        let mut legacy = args();
        legacy.workstream_id = "owner/repo#7".to_owned();
        assert!(validate_args(&legacy).is_ok());

        for invalid_id in ["owner/repo#8", "owner/other#7", "OWNER/repo#7"] {
            let mut invalid = legacy.clone();
            invalid.workstream_id = invalid_id.to_owned();
            assert!(validate_args(&invalid).is_err(), "accepted {invalid_id:?}");
        }

        let mut managed = legacy.clone();
        managed.goal_managed = true;
        assert!(validate_args(&managed).is_err());

        let mut routed = legacy;
        routed.agent_provider = Some("codex".to_owned());
        routed.agent_session_id = Some("session-7".to_owned());
        assert!(validate_args(&routed).is_err());
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
                "ambient goal-managed Codex",
                AgentEnvironment {
                    codex_session: Some("codex-managed".to_owned()),
                    goal_managed: true,
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
            (
                "ambient goal-managed HerdR Codex",
                AgentEnvironment {
                    codex_session: Some("codex-managed-herdr".to_owned()),
                    goal_managed: true,
                    herdr_env: Some("1".to_owned()),
                    herdr_session: Some("herdr-session".to_owned()),
                    herdr_workspace_id: Some("workspace".to_owned()),
                    herdr_tab_id: Some("tab".to_owned()),
                    herdr_pane_id: Some("pane".to_owned()),
                    ..AgentEnvironment::default()
                },
            ),
            (
                "ambient goal-managed cmux Claude",
                AgentEnvironment {
                    claude_session: Some("claude-managed-cmux".to_owned()),
                    surface_id: Some("surface".to_owned()),
                    goal_managed: true,
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

        let goal_only = AgentEnvironment {
            goal_managed: true,
            ..AgentEnvironment::default()
        };
        let error = resolve_handoff_agent(&legacy, |args| {
            resolve_agent_context_with_environment(args, &goal_only)
        })
        .expect_err("ambient managed lifecycle without an agent must be refused");
        assert!(
            error
                .message()
                .contains("--goal-managed requires a resumable agent session")
        );
    }

    #[test]
    fn agent_identity_requires_a_complete_provider_session_pair() {
        let mut managed = args();
        managed.goal_managed = true;
        managed.after_handoff = "pause".to_owned();
        managed.agent_provider = Some("codex".to_owned());
        assert!(resolve_agent_context(&managed).is_err());

        managed.agent_session_id = Some("019d-test-thread".to_owned());
        let context =
            resolve_agent_context_with_environment(&managed, &AgentEnvironment::default())
                .expect("valid context")
                .expect("captured context");
        assert_eq!(context.provider, "codex");
        assert_eq!(context.resume_transport, "codex_queue");
        assert!(context.goal_managed);
        assert_eq!(context.goal_lifecycle, GoalLifecycle::Managed);
        assert_eq!(context.goal_status, GoalStatus::Unknown);
        assert_eq!(
            context.goal_status_provenance,
            GoalStatusProvenance::NotObserved
        );
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
    fn pause_without_a_task_graph_fails_before_transport_and_dry_run_is_truthful() {
        let mut paused = explicit_agent_args("codex", "paused-session");
        paused.goal_managed = true;
        paused.after_handoff = "pause".to_owned();
        let agent = resolve_agent_context_with_environment(&paused, &AgentEnvironment::default())
            .expect("resolve agent")
            .expect("agent");
        let route = agent_route_reference(&agent, "m3");
        let error = prepare_handoff_receipt_with_profile_and_disposition(
            None,
            &paused,
            "owner/repo",
            "m3",
            Some(route.clone()),
            None,
            None,
        )
        .expect_err("pause cannot be prepared without task-graph authority");
        assert!(error.message().contains("task-graph"));

        paused.apply = false;
        let mut output = Vec::new();
        render(
            &paused,
            "owner/repo",
            Some(&route),
            "m3",
            true,
            false,
            AgentDisposition::Continue,
            false,
            &mut output,
        )
        .expect("render dry run");
        let value: Value = serde_json::from_slice(&output).expect("dry-run json");
        assert_eq!(value["requested_agent_disposition"], "pause");
        assert_eq!(value["agent_disposition"], "continue");
        assert_eq!(value["agent_disposition_supported"], true);
        assert_eq!(value["pause_supported"], true);
        assert_eq!(value["pause_required"], false);
        assert_eq!(value["wake_consumer_available"], false);
        assert_eq!(value["monitoring_transferred"], false);
    }

    #[test]
    fn ambiguous_provider_environment_and_explicit_orphan_route_fields_fail_closed() {
        let environment = AgentEnvironment {
            codex_session: Some("codex-session".to_owned()),
            claude_session: Some("claude-session".to_owned()),
            surface_id: None,
            goal_managed: false,
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
        assert_eq!(handoff.goal_lifecycle, GoalLifecycle::Unmanaged);
        assert_eq!(handoff.goal_status, GoalStatus::Unmanaged);
        assert_eq!(
            handoff.goal_status_provenance,
            GoalStatusProvenance::NotObserved
        );
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
        managed.goal_managed = true;
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
        assert_eq!(receipt.agent_disposition, AgentDisposition::Continue);
        assert!(!receipt.pause_required);
        assert!(!receipt.wake_consumer_available);
        assert_eq!(receipt.goal_lifecycle, GoalLifecycle::Managed);
        assert_eq!(receipt.goal_status, GoalStatus::Unknown);
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
    fn same_owner_replay_rejects_disposition_change() {
        let mut first = explicit_agent_args("codex", "stable-session");
        first.goal_managed = true;
        let route = route_for(&first, "m3");
        let receipt =
            prepare_handoff_receipt(None, &first, "owner/repo", "m3", Some(route.clone()))
                .expect("first handoff");
        let temp = tempfile::tempdir().expect("temp");
        let receipt = persist_handoff(
            &temp.path().join("receipt.json"),
            receipt,
            HandoffPhase::Intent,
        )
        .expect("persist receipt");

        let mut changed = first;
        changed.after_handoff = "pause".to_owned();
        let error =
            prepare_handoff_receipt(Some(receipt), &changed, "owner/repo", "m3", Some(route))
                .expect_err("disposition cannot change on replay");
        assert!(error.message().contains("task-graph proof"));
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

#[cfg(all(test, unix))]
#[path = "handoff/launch_profile_tests.rs"]
mod launch_profile_tests;
