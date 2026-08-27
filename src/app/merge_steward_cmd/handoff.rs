use super::{
    CliFailure, GitHubActions, HANDOFF_CONTEXT, MANAGED_LABEL, Path, UNMANAGED_LABEL, Value, Write,
    gh_json, is_full_sha, observation::encode_path_segment, resolve_repos, write_json_envelope,
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
    goal_lifecycle: GoalLifecycle,
    goal_status: GoalStatus,
    goal_status_provenance: GoalStatusProvenance,
    phase: HandoffPhase,
    agent_disposition: String,
    pause_required: bool,
    wake_consumer_available: bool,
    created_at: String,
    updated_at: String,
}

pub(crate) fn steward_handoff_command<W: Write>(
    args: &StewardHandoffArgs,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    validate_args(args)?;
    let repo = resolve_repos(args.repo.clone().into_iter().collect(), cwd)?
        .into_iter()
        .next()
        .ok_or_else(|| CliFailure::new(1, "repository was not resolved"))?;
    verify_exact_open_pr(actions, &repo, args.pr, &args.head)?;
    let agent = resolve_agent_context(args)?;
    let origin_machine = if args.apply {
        resolve_origin_machine(runtime_paths)?
    } else {
        preview_origin_machine(runtime_paths)?
    };
    let agent_route = agent
        .as_ref()
        .map(|agent| agent_route_reference(agent, &origin_machine));

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
        remove_label(actions, &repo, args.pr, UNMANAGED_LABEL)?;
        debug_assert_eq!(receipt.phase, HandoffPhase::Managed);
    }

    render(
        args,
        &repo,
        agent_route.as_ref(),
        &origin_machine,
        json_output,
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
    let workstream = args.workstream_id.trim();
    if workstream.is_empty()
        || workstream.len() > 124
        || workstream.chars().any(char::is_whitespace)
    {
        return Err(CliFailure::new(
            1,
            "--workstream-id must be 1-124 non-whitespace characters",
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
    if !matches!(args.after_handoff.as_str(), "continue" | "pause") {
        return Err(CliFailure::new(
            1,
            "--after-handoff must be continue or pause",
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
    if args.apply && args.after_handoff == "pause" {
        return Err(CliFailure::new(
            1,
            "--after-handoff pause is unavailable until a scheduler wake consumer is deployed; use continue",
        ));
    }
    Ok(())
}

#[derive(Default)]
struct AgentEnvironment {
    codex_session: Option<String>,
    claude_session: Option<String>,
    surface_id: Option<String>,
    goal_managed: bool,
}

fn resolve_agent_context(
    args: &StewardHandoffArgs,
) -> Result<Option<AgentResumeContext>, CliFailure> {
    resolve_agent_context_with_environment(
        args,
        &AgentEnvironment {
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
        },
    )
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
    let route_id = opaque_id(
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
    );
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
    let pause_required = args.after_handoff == "pause"
        && agent_route.as_ref().is_some_and(|route| route.goal_managed);
    if let Some(existing) = existing {
        validate_existing_handoff(&existing, args, &normalized_repo, &normalized_head)?;
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
                pause_required,
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
        if existing.agent_disposition != args.after_handoff
            || existing.pause_required != pause_required
        {
            return Err(CliFailure::new(
                1,
                "same-owner handoff cannot change agent disposition or pause intent",
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
        goal_lifecycle,
        goal_status,
        goal_status_provenance,
        pause_required,
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
    goal_lifecycle: GoalLifecycle,
    goal_status: GoalStatus,
    goal_status_provenance: GoalStatusProvenance,
    pause_required: bool,
) -> DurableStewardHandoff {
    let now = Utc::now().to_rfc3339();
    DurableStewardHandoff {
        schema_version: 2,
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
        goal_lifecycle,
        goal_status,
        goal_status_provenance,
        agent_disposition: args.after_handoff.clone(),
        pause_required,
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
    if existing.schema_version != 2
        || !existing.repo.eq_ignore_ascii_case(normalized_repo)
        || existing.pr != args.pr
        || !existing.head_sha.eq_ignore_ascii_case(normalized_head)
        || existing.ownership_generation == 0
        || existing.revision == 0
        || existing.wake_consumer_available
    {
        return Err(CliFailure::new(
            1,
            "durable handoff receipt is incompatible or does not match its exact-head path",
        ));
    }
    Ok(())
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
    pause_required: bool,
) -> Result<DurableStewardHandoff, CliFailure> {
    if agent_route.is_none() {
        return Err(CliFailure::new(
            1,
            "--transfer-agent-owner requires an explicit replacement agent route",
        ));
    }
    if existing.workstream_id != args.workstream_id
        || existing.context_url != args.context_url
        || existing.agent_disposition != args.after_handoff
        || existing.pause_required != pause_required
    {
        return Err(CliFailure::new(
            1,
            "ownership transfer cannot change workstream, context, or disposition",
        ));
    }
    if existing.owner_id == owner_id && existing.agent_route == agent_route {
        return Ok(existing);
    }
    existing.owner_id = owner_id;
    existing.agent_route = agent_route;
    origin_machine.clone_into(&mut existing.origin_machine);
    existing.repair_route = RepairRoute::OriginalAgent;
    existing.goal_lifecycle = goal_lifecycle;
    existing.goal_status = goal_status;
    existing.goal_status_provenance = goal_status_provenance;
    existing.ownership_generation = existing
        .ownership_generation
        .checked_add(1)
        .ok_or_else(|| CliFailure::new(1, "handoff ownership generation overflow"))?;
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
        && existing.goal_managed == incoming.goal_managed
        && existing.goal_lifecycle == incoming.goal_lifecycle
        && existing.goal_status == incoming.goal_status
        && existing.goal_status_provenance == incoming.goal_status_provenance
        && existing.resume_transport == incoming.resume_transport
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
    run_steward_write(actions, &command, "handoff receipt")
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
            "steward label creation",
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
        "steward label attachment",
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
        "steward label removal",
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
    purpose: &str,
) -> Result<String, crate::cloud::GitHubError> {
    match actions.run_gh(args) {
        Ok(value) => Ok(value),
        Err(error) if error.is_integration_permission_denial() => {
            let _ = crate::writer_domain_lease::write_stderr(format_args!(
                "shipyard: configured GitHub App cannot write {purpose}; falling back to ambient gh auth for this steward mutation only."
            ));
            actions.run_gh_ambient(args)
        }
        Err(error) => Err(error),
    }
}

fn render<W: Write>(
    args: &StewardHandoffArgs,
    repo: &str,
    agent_route: Option<&AgentRouteReference>,
    origin_machine: &str,
    json_output: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let pause_requested = args.after_handoff == "pause";
    let effective_disposition = if pause_requested {
        "unsupported"
    } else {
        args.after_handoff.as_str()
    };
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("apply".to_owned(), Value::from(args.apply));
        data.insert("repo".to_owned(), Value::from(repo));
        data.insert("pr".to_owned(), Value::from(args.pr));
        data.insert("head_sha".to_owned(), Value::from(args.head.clone()));
        data.insert(
            "workstream_id".to_owned(),
            Value::from(args.workstream_id.clone()),
        );
        data.insert("managed_label".to_owned(), Value::from(MANAGED_LABEL));
        data.insert("handoff_context".to_owned(), Value::from(HANDOFF_CONTEXT));
        data.insert("monitoring_transferred".to_owned(), Value::from(false));
        data.insert(
            "agent_disposition".to_owned(),
            Value::from(effective_disposition),
        );
        data.insert(
            "requested_agent_disposition".to_owned(),
            Value::from(args.after_handoff.clone()),
        );
        data.insert(
            "agent_disposition_supported".to_owned(),
            Value::from(!pause_requested),
        );
        data.insert("pause_required".to_owned(), Value::from(false));
        data.insert("pause_supported".to_owned(), Value::from(false));
        data.insert("wake_consumer_available".to_owned(), Value::from(false));
        data.insert(
            "origin_machine".to_owned(),
            Value::from(origin_machine.to_owned()),
        );
        data.insert(
            "repair_route".to_owned(),
            Value::from(if agent_route.is_some() {
                "original_agent"
            } else {
                "fresh_agent_only"
            }),
        );
        data.insert(
            "goal_lifecycle".to_owned(),
            Value::from(if agent_route.is_some_and(|route| route.goal_managed) {
                "managed"
            } else {
                "unmanaged"
            }),
        );
        data.insert(
            "goal_status".to_owned(),
            serde_json::to_value(
                agent_route.map_or(GoalStatus::Unmanaged, |route| route.goal_status),
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "goal_status_provenance".to_owned(),
            Value::from("not_observed"),
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
        return write_json_envelope(stdout, "runner.steward-handoff", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "steward handoff: mode={} repo={} pr=#{} head={} workstream={} label={} requested_disposition={} disposition={} disposition_supported={} pause_supported=false pause_required=false wake_consumer_available=false origin_machine={} repair_route={}",
        if args.apply { "apply" } else { "dry-run" },
        repo,
        args.pr,
        args.head,
        args.workstream_id,
        MANAGED_LABEL,
        args.after_handoff,
        effective_disposition,
        !pause_requested,
        origin_machine,
        if agent_route.is_some() {
            "original_agent"
        } else {
            "fresh_agent_only"
        }
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
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
    fn workstream_identifier_is_small_and_single_token() {
        let mut invalid = args();
        invalid.workstream_id = "GEN 7".to_owned();
        assert!(validate_args(&invalid).is_err());
        assert!(validate_args(&args()).is_ok());
    }

    #[test]
    fn agent_identity_requires_a_complete_provider_session_pair() {
        let mut managed = args();
        managed.goal_managed = true;
        managed.after_handoff = "pause".to_owned();
        managed.agent_provider = Some("codex".to_owned());
        assert!(resolve_agent_context(&managed).is_err());

        managed.agent_session_id = Some("019d-test-thread".to_owned());
        let context = resolve_agent_context(&managed)
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
    fn applied_pause_fails_before_transport_and_dry_run_is_truthful() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = RuntimePaths::current_with_overrides(
            crate::identity::RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut paused = explicit_agent_args("codex", "paused-session");
        paused.goal_managed = true;
        paused.after_handoff = "pause".to_owned();
        paused.apply = true;
        let error = steward_handoff_command(
            &paused,
            temp.path(),
            &paths,
            &GitHubActions::new(temp.path()),
            false,
            &mut Vec::new(),
        )
        .expect_err("pause cannot apply without a wake consumer");
        assert!(error.message().contains("wake consumer"));

        paused.apply = false;
        let agent = resolve_agent_context_with_environment(&paused, &AgentEnvironment::default())
            .expect("resolve dry-run agent")
            .expect("dry-run agent");
        let route = agent_route_reference(&agent, "m3");
        let mut output = Vec::new();
        render(&paused, "owner/repo", Some(&route), "m3", true, &mut output)
            .expect("render dry run");
        let value: Value = serde_json::from_slice(&output).expect("dry-run json");
        assert_eq!(value["requested_agent_disposition"], "pause");
        assert_eq!(value["agent_disposition"], "unsupported");
        assert_eq!(value["agent_disposition_supported"], false);
        assert_eq!(value["pause_supported"], false);
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
        let receipt = persist_handoff(&path, replayed_intent, HandoffPhase::Managed)
            .expect("persist managed");
        assert_eq!(receipt.phase, HandoffPhase::Managed);
        assert_eq!(receipt.revision, 4);
        assert_eq!(receipt.head_sha, managed.head);
        assert_eq!(receipt.agent_disposition, "continue");
        assert!(!receipt.pause_required);
        assert!(!receipt.wake_consumer_available);
        assert_eq!(receipt.goal_lifecycle, GoalLifecycle::Managed);
        assert_eq!(receipt.goal_status, GoalStatus::Unknown);

        let public_bytes = std::fs::read_to_string(&path).expect("read receipt");
        assert!(!public_bytes.contains("session-7"));
        assert!(!public_bytes.contains("coordinator-1"));
        assert!(!public_bytes.contains("surface-7"));
        let private_bytes = std::fs::read_to_string(&route_path).expect("read route");
        assert!(private_bytes.contains("session-7"));
        assert!(private_bytes.contains("coordinator-1"));
        assert!(private_bytes.contains("surface-7"));

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
        assert!(
            error
                .message()
                .contains("agent disposition or pause intent")
        );
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
        let error = steward_handoff_command(
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

        steward_handoff_command(
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
        steward_handoff_command(
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
        steward_handoff_command(
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
        steward_handoff_command(
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
        steward_handoff_command(
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
    fn exact_integration_permission_error_uses_one_ambient_fallback() {
        let temp = tempfile::tempdir().expect("temp");
        let (actions, count) = sequenced_gh(&temp, "Resource not accessible by integration");
        run_steward_write(&actions, &["api".to_owned(), "test".to_owned()], "test")
            .expect("ambient fallback");
        assert_eq!(std::fs::read_to_string(count).expect("count"), "2");
    }

    #[cfg(unix)]
    #[test]
    fn generic_write_failure_does_not_escape_to_ambient_auth() {
        let temp = tempfile::tempdir().expect("temp");
        let (actions, count) = sequenced_gh(&temp, "HTTP 403 generic forbidden");
        assert!(
            run_steward_write(&actions, &["api".to_owned(), "test".to_owned()], "test").is_err()
        );
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
