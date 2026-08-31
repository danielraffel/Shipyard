//! Bounded rollout execution and typed post-install evidence collection.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::auth_support;
use super::{
    COMPANION_BINARY_NAME, HOST_UPDATE_TIMEOUT, HostUpdatePlan,
    REMOTE_AFTER_COMPANION_SHA256_PREFIX, REMOTE_AFTER_COMPANION_VERSION_PREFIX,
    REMOTE_AFTER_PRIMARY_SHA256_PREFIX, REMOTE_AFTER_PRIMARY_VERSION_PREFIX,
    REMOTE_AFTER_STATUS_PREFIX, REMOTE_AUTHORITY_ID_PREFIX, REMOTE_BEFORE_COMPANION_SHA256_PREFIX,
    REMOTE_BEFORE_COMPANION_VERSION_PREFIX, REMOTE_BEFORE_PRIMARY_SHA256_PREFIX,
    REMOTE_BEFORE_PRIMARY_VERSION_PREFIX, REMOTE_BEFORE_STATUS_PREFIX, REMOTE_REFRESH_PREFIX,
    REMOTE_RELEASE_ASSET_SHA256_PREFIX, tag_requires_companion, tag_supports_auth_resolver,
};
use crate::paths::{home_dir, unattended_tool_path};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HostUpdateEvidence {
    pub(super) release_authority_identity: String,
    pub(super) release_asset_sha256: String,
    pub(super) before_pair: BinaryPairEvidence,
    pub(super) after_pair: BinaryPairEvidence,
    pub(super) auth_support_before: AuthSupportEvidence,
    pub(super) auth_support_after: AuthSupportEvidence,
    pub(super) executable_sha256: String,
    pub(super) cli_version: String,
    pub(super) daemon_version: String,
    pub(super) daemon_pid: u32,
    pub(super) daemon_runtime: DaemonRuntimeEvidence,
    pub(super) configured_repos_before: Option<Vec<String>>,
    pub(super) configured_repos_after: Vec<String>,
    pub(super) configured_repos_preserved: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct DaemonRuntimeEvidence {
    pub(super) pid: u32,
    pub(super) loaded_executable_path: PathBuf,
    pub(super) loaded_executable_sha256: String,
    pub(super) rendered_launch_sha256: String,
    pub(super) loaded_launch_sha256: String,
    pub(super) machine_auth_probe_sha256: String,
    pub(super) machine_auth_generation_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct AuthSupportEvidence {
    pub(super) helper: SupportFileEvidence,
    pub(super) wrapper: SupportFileEvidence,
    pub(super) generation: Option<GenerationEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct GenerationEvidence {
    pub(super) generation_contract: String,
    pub(super) generation_id: String,
    pub(super) authority_identity: String,
    pub(super) selector_path: PathBuf,
    pub(super) selector_target: PathBuf,
    pub(super) selector_recheck_target: PathBuf,
    pub(super) manifest: GenerationMemberEvidence,
    pub(super) helper: GenerationMemberEvidence,
    pub(super) wrapper: GenerationMemberEvidence,
    pub(super) binary: GenerationMemberEvidence,
    pub(super) companion: Option<GenerationMemberEvidence>,
    pub(super) context: Option<GenerationMemberEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct GenerationMemberEvidence {
    pub(super) path: PathBuf,
    pub(super) sha256: String,
    pub(super) mode: u32,
}

pub(super) const REMOTE_GENERATION_SELECTOR_PREFIX: &str = "SHIPYARD_FLEET_GENERATION_SELECTOR=";
pub(super) const REMOTE_GENERATION_SELECTOR_RECHECK_PREFIX: &str =
    "SHIPYARD_FLEET_GENERATION_SELECTOR_RECHECK=";
pub(super) const REMOTE_GENERATION_ID_PREFIX: &str = "SHIPYARD_FLEET_GENERATION_ID=";
pub(super) const REMOTE_GENERATION_CONTRACT_PREFIX: &str = "SHIPYARD_FLEET_GENERATION_CONTRACT=";
pub(super) const REMOTE_GENERATION_AUTHORITY_PREFIX: &str = "SHIPYARD_FLEET_GENERATION_AUTHORITY=";
pub(super) const REMOTE_GENERATION_MANIFEST_SHA_PREFIX: &str =
    "SHIPYARD_FLEET_GENERATION_MANIFEST_SHA256=";
pub(super) const REMOTE_GENERATION_HELPER_SHA_PREFIX: &str =
    "SHIPYARD_FLEET_GENERATION_HELPER_SHA256=";
pub(super) const REMOTE_GENERATION_WRAPPER_SHA_PREFIX: &str =
    "SHIPYARD_FLEET_GENERATION_WRAPPER_SHA256=";
pub(super) const REMOTE_GENERATION_BINARY_SHA_PREFIX: &str =
    "SHIPYARD_FLEET_GENERATION_BINARY_SHA256=";
pub(super) const REMOTE_GENERATION_COMPANION_SHA_PREFIX: &str =
    "SHIPYARD_FLEET_GENERATION_COMPANION_SHA256=";
pub(super) const REMOTE_GENERATION_CONTEXT_SHA_PREFIX: &str =
    "SHIPYARD_FLEET_GENERATION_CONTEXT_SHA256=";
pub(super) const REMOTE_DAEMON_PID_PREFIX: &str = "SHIPYARD_FLEET_DAEMON_PID=";
pub(super) const REMOTE_DAEMON_EXECUTABLE_PREFIX: &str = "SHIPYARD_FLEET_DAEMON_EXECUTABLE=";
pub(super) const REMOTE_DAEMON_EXECUTABLE_SHA_PREFIX: &str =
    "SHIPYARD_FLEET_DAEMON_EXECUTABLE_SHA256=";
pub(super) const REMOTE_DAEMON_LAUNCH_PREFIX: &str = "SHIPYARD_FLEET_DAEMON_LAUNCH=";
pub(super) const REMOTE_DAEMON_AUTH_PROBE_SHA_PREFIX: &str =
    "SHIPYARD_FLEET_DAEMON_AUTH_PROBE_SHA256=";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SupportFileEvidence {
    pub(super) path: PathBuf,
    pub(super) generation_target: Option<PathBuf>,
    pub(super) sha256: Option<String>,
    pub(super) mode: Option<u32>,
    pub(super) source_blob_oid: Option<String>,
    pub(super) source_identity: Option<String>,
    pub(super) source_identity_basis: SourceIdentityBasis,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct BinaryEvidence {
    pub(super) path: PathBuf,
    pub(super) semantic_version: String,
    pub(super) sha256: String,
    pub(super) source_identity: Option<String>,
    pub(super) source_identity_basis: SourceIdentityBasis,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SourceIdentityBasis {
    UnverifiedPreinstall,
    VerifiedReleaseAuthority,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct BinaryPairEvidence {
    pub(super) primary: BinaryEvidence,
    pub(super) companion: Option<BinaryEvidence>,
}

#[derive(Debug)]
pub(super) enum PlanExecutionError {
    TimedOut(String),
    Failed(String),
}

pub(super) fn execute_plan(
    plan: &HostUpdatePlan,
) -> Result<HostUpdateEvidence, PlanExecutionError> {
    execute_plan_with_timeout(plan, HOST_UPDATE_TIMEOUT)
}

pub(super) fn execute_plan_with_timeout(
    plan: &HostUpdatePlan,
    timeout: Duration,
) -> Result<HostUpdateEvidence, PlanExecutionError> {
    let deadline = Instant::now() + timeout;
    let (before_status, before_pair, before_auth) = if plan.ssh.is_none() {
        let status = run_local_daemon_status(plan, deadline)?;
        let pair = collect_local_pair(plan, deadline, false)?;
        validate_binary_pair(plan, &pair, None).map_err(PlanExecutionError::Failed)?;
        let auth = collect_local_auth_support(plan, false)?;
        (Some(status), Some(pair), Some(auth))
    } else {
        (None, None, None)
    };
    let mut command = if let Some(host) = &plan.ssh {
        let mut command = Command::new(ssh_binary());
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "StrictHostKeyChecking=yes",
        ]);
        command.arg(host).arg(&plan.command);
        command
    } else {
        let mut command = Command::new("/bin/bash");
        command
            .args(["-c", &plan.command])
            .env_clear()
            .env("HOME", home_dir())
            .env("PATH", unattended_tool_path());
        command
    };
    let output = run_bounded_output(
        &mut command,
        deadline,
        &format!("fleet update for host class {}", plan.class),
    )?;
    if !output.status.success() {
        return Err(PlanExecutionError::Failed(format!(
            "update command exited {}",
            output.status.code().unwrap_or(-1)
        )));
    }
    if plan.ssh.is_some() {
        parse_remote_evidence(plan, &output.stdout)
    } else {
        collect_local_evidence(
            plan,
            &before_status.expect("local status was collected before update"),
            before_pair.expect("local pair was collected before update"),
            before_auth.expect("local auth support was collected before update"),
            &output.stdout,
            deadline,
        )
    }
}

fn run_bounded_output(
    command: &mut Command,
    deadline: Instant,
    label: &str,
) -> Result<Output, PlanExecutionError> {
    crate::process::run_output_until(command, deadline, label).map_err(|error| match error {
        crate::process::BoundedOutputError::TimedOut { .. } => PlanExecutionError::TimedOut(
            format!("{label} exhausted the bounded host-attempt deadline"),
        ),
        other => PlanExecutionError::Failed(other.to_string()),
    })
}

fn run_local_daemon_status(
    plan: &HostUpdatePlan,
    host_deadline: Instant,
) -> Result<Value, PlanExecutionError> {
    let mut command = Command::new(&plan.binary);
    command
        .args(["--mode", plan.runtime_mode.as_str(), "--global-dir"])
        .arg(&plan.global_dir)
        .arg("--state-dir")
        .arg(&plan.state_dir)
        .args(["--json", "daemon", "status"])
        .env_clear()
        .env("HOME", home_dir())
        .env("PATH", unattended_tool_path());
    let output = run_bounded_output(
        &mut command,
        probe_deadline(host_deadline),
        &format!("daemon status for host class {}", plan.class),
    )?;
    if !output.status.success() {
        return Err(PlanExecutionError::Failed(format!(
            "daemon status exited {}",
            output.status.code().unwrap_or(-1)
        )));
    }
    parse_json_value(&output.stdout, "local daemon status")
}

fn collect_local_evidence(
    plan: &HostUpdatePlan,
    before_status: &Value,
    before_pair: BinaryPairEvidence,
    before_auth: AuthSupportEvidence,
    update_stdout: &[u8],
    host_deadline: Instant,
) -> Result<HostUpdateEvidence, PlanExecutionError> {
    let after_pair = collect_local_pair(plan, host_deadline, true)?;
    let after_auth = collect_local_auth_support(plan, true)?;
    let after_status = run_local_daemon_status(plan, host_deadline)?;
    let daemon_pid = local_refresh_daemon_pid_from_output(update_stdout).ok_or_else(|| {
        PlanExecutionError::Failed(
            "local update returned no typed nonzero daemon PID receipt".to_owned(),
        )
    })?;
    let generation = after_auth.generation.as_ref().ok_or_else(|| {
        PlanExecutionError::Failed("local update omitted auth generation".to_owned())
    })?;
    let daemon_runtime =
        collect_local_daemon_runtime(plan, daemon_pid, generation, &after_status, host_deadline)?;
    evidence_from_values(
        before_pair,
        after_pair,
        before_auth,
        after_auth,
        daemon_pid,
        daemon_runtime,
        before_status,
        &after_status,
        plan.release_authority.identity_sha256.clone(),
        plan.release_authority.platform_asset.sha256.clone(),
    )
}

fn local_refresh_daemon_pid_from_output(update_stdout: &[u8]) -> Option<u32> {
    serde_json::Deserializer::from_slice(update_stdout)
        .into_iter::<Value>()
        .filter_map(Result::ok)
        .find_map(|value| local_refresh_daemon_pid(&value))
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
}

fn collect_local_daemon_runtime(
    plan: &HostUpdatePlan,
    daemon_pid: u32,
    generation: &GenerationEvidence,
    after_status: &Value,
    host_deadline: Instant,
) -> Result<DaemonRuntimeEvidence, PlanExecutionError> {
    let pid_path = plan.state_dir.join("daemon/daemon.pid");
    let recorded_pid = std::fs::read_to_string(&pid_path)
        .map_err(|error| {
            PlanExecutionError::Failed(format!("failed to read {}: {error}", pid_path.display()))
        })?
        .trim()
        .parse::<u32>()
        .map_err(|_| PlanExecutionError::Failed("daemon PID file was invalid".to_owned()))?;
    if recorded_pid != daemon_pid {
        return Err(PlanExecutionError::Failed(
            "refreshed daemon PID disagreed with the live daemon PID file".to_owned(),
        ));
    }
    let mut lsof = Command::new("/usr/sbin/lsof");
    lsof.args(["-a", "-p", &daemon_pid.to_string(), "-d", "txt", "-Fn"]);
    let lsof = run_bounded_output(
        &mut lsof,
        probe_deadline(host_deadline),
        "daemon loaded executable",
    )?;
    if !lsof.status.success() {
        return Err(PlanExecutionError::Failed(
            "failed to inspect daemon loaded executable".to_owned(),
        ));
    }
    let loaded_executable_path = parse_lsof_text_path(&lsof.stdout)?;
    let loaded_executable_sha256 = sha256_file(&loaded_executable_path).map_err(|error| {
        PlanExecutionError::Failed(format!("failed to hash daemon loaded executable: {error}"))
    })?;
    let mut ps = Command::new("/bin/ps");
    ps.args(["-ww", "-p", &daemon_pid.to_string(), "-o", "command="]);
    let ps = run_bounded_output(
        &mut ps,
        probe_deadline(host_deadline),
        "daemon loaded launch",
    )?;
    if !ps.status.success() {
        return Err(PlanExecutionError::Failed(
            "failed to inspect daemon launch command".to_owned(),
        ));
    }
    let loaded_launch = single_line(&ps.stdout, "daemon launch command")?;
    let rendered_launch = rendered_daemon_launch(plan, &configured_repos(after_status)?);
    let mut auth = Command::new(&generation.binary.path);
    auth.args(["--mode", plan.runtime_mode.as_str(), "--global-dir"])
        .arg(&plan.global_dir)
        .arg("--state-dir")
        .arg(&plan.state_dir)
        .args(["auth", "helper-argv", "--wrapper"])
        .arg(&plan.auth_wrapper)
        .args(["--repo", &plan.release_authority.repository])
        .env_clear()
        .env("HOME", home_dir())
        .env("PATH", unattended_tool_path());
    let auth = run_bounded_output(
        &mut auth,
        probe_deadline(host_deadline),
        "machine-global auth launch",
    )?;
    if !auth.status.success() {
        return Err(PlanExecutionError::Failed(
            "machine-global auth launch probe failed".to_owned(),
        ));
    }
    validate_auth_launch_probe(plan, &auth.stdout)?;
    let final_pid = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok());
    let final_selector = std::fs::read_link(&plan.auth_wrapper).ok();
    if final_pid != Some(daemon_pid) || final_selector.as_ref() != Some(&generation.selector_target)
    {
        return Err(PlanExecutionError::Failed(
            "daemon PID or auth selector changed while runtime evidence was collected".to_owned(),
        ));
    }
    Ok(DaemonRuntimeEvidence {
        pid: daemon_pid,
        loaded_executable_path,
        loaded_executable_sha256,
        rendered_launch_sha256: sha256_bytes(rendered_launch.as_bytes()),
        loaded_launch_sha256: sha256_bytes(loaded_launch.as_bytes()),
        machine_auth_probe_sha256: sha256_bytes(&auth.stdout),
        machine_auth_generation_id: generation.generation_id.clone(),
    })
}

fn parse_lsof_text_path(stdout: &[u8]) -> Result<PathBuf, PlanExecutionError> {
    let text = std::str::from_utf8(stdout)
        .map_err(|_| PlanExecutionError::Failed("daemon lsof output was not UTF-8".to_owned()))?;
    let mut saw_text = false;
    for line in text.lines() {
        if line == "ftxt" {
            saw_text = true;
        } else if saw_text && line.starts_with('n') && line.len() > 1 {
            return Ok(PathBuf::from(&line[1..]));
        }
    }
    Err(PlanExecutionError::Failed(
        "daemon lsof output omitted the loaded text executable".to_owned(),
    ))
}

fn single_line(stdout: &[u8], label: &str) -> Result<String, PlanExecutionError> {
    let text = std::str::from_utf8(stdout)
        .map_err(|_| PlanExecutionError::Failed(format!("{label} was not UTF-8")))?;
    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() != 1 || lines[0].is_empty() {
        return Err(PlanExecutionError::Failed(format!(
            "{label} was not one nonempty line"
        )));
    }
    Ok(lines[0].to_owned())
}

fn validate_auth_launch_probe(
    plan: &HostUpdatePlan,
    stdout: &[u8],
) -> Result<(), PlanExecutionError> {
    let value = parse_json_value(stdout, "machine-global auth launch probe")?;
    let Some(object) = value.as_object() else {
        return Err(invalid_auth_launch_probe());
    };
    let expected_keys = [
        "schema_version",
        "command",
        "wrapper",
        "repo",
        "credential_argv",
    ];
    let argv = object.get("credential_argv").and_then(Value::as_array);
    if object.len() != expected_keys.len()
        || expected_keys.iter().any(|key| !object.contains_key(*key))
        || object.get("schema_version").and_then(Value::as_u64) != Some(1)
        || object.get("command").and_then(Value::as_str) != Some("auth.helper-argv")
        || value.get("wrapper").and_then(Value::as_str) != plan.auth_wrapper.to_str()
        || value.get("repo").and_then(Value::as_str)
            != Some(plan.release_authority.repository.as_str())
        || !argv.is_some_and(|argv| argv.len() == 4 && argv.iter().all(Value::is_string))
    {
        return Err(invalid_auth_launch_probe());
    }
    let argv = argv.expect("validated credential argv");
    let app_id = argv[1].as_str().expect("validated string argv");
    let private_key = argv[3].as_str().expect("validated string argv");
    if argv[0].as_str() != Some("--app-id")
        || argv[2].as_str() != Some("--private-key")
        || !valid_app_id(app_id)
        || !normalized_absolute_credential_path(private_key)
        || argv
            .iter()
            .filter_map(Value::as_str)
            .any(|item| item.is_empty() || item.chars().any(char::is_control))
    {
        return Err(invalid_auth_launch_probe());
    }
    Ok(())
}

fn invalid_auth_launch_probe() -> PlanExecutionError {
    PlanExecutionError::Failed(
        "machine-global auth launch probe did not bind the exact typed credential contract"
            .to_owned(),
    )
}

fn valid_app_id(value: &str) -> bool {
    (1..=20).contains(&value.len())
        && value.is_ascii()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|parsed| parsed > 0)
}

fn normalized_absolute_credential_path(value: &str) -> bool {
    (2..=4096).contains(&value.len())
        && value.starts_with('/')
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn rendered_daemon_launch(plan: &HostUpdatePlan, repos: &[String]) -> String {
    let mut repos = repos.to_vec();
    repos.sort();
    repos.dedup();
    let mut rendered = format!(
        "{} --mode {} --global-dir {} --state-dir {} daemon run",
        plan.binary.display(),
        plan.runtime_mode.as_str(),
        plan.global_dir.display(),
        plan.state_dir.display()
    );
    for repo in repos {
        rendered.push_str(" --repo ");
        rendered.push_str(&repo);
    }
    rendered
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn local_refresh_daemon_pid(value: &Value) -> Option<u64> {
    if value.get("event").and_then(Value::as_str) == Some("daemon_refreshed") {
        return value.get("daemon_pid").and_then(Value::as_u64);
    }
    (value.get("command").and_then(Value::as_str) == Some("daemon:refresh"))
        .then(|| value.get("new_pid").and_then(Value::as_u64))
        .flatten()
}

fn collect_local_auth_support(
    plan: &HostUpdatePlan,
    verified: bool,
) -> Result<AuthSupportEvidence, PlanExecutionError> {
    let helper = collect_local_support_file(
        &plan.auth_helper,
        verified.then_some(plan.release_authority.auth_helper.blob_oid.as_str()),
        verified,
        plan,
    )?;
    let wrapper = collect_local_support_file(
        &plan.auth_wrapper,
        verified.then_some(plan.release_authority.auth_wrapper.blob_oid.as_str()),
        verified,
        plan,
    )?;
    let generation = verified
        .then(|| collect_local_generation(plan))
        .transpose()?;
    Ok(AuthSupportEvidence {
        helper,
        wrapper,
        generation,
    })
}

fn collect_local_generation(
    plan: &HostUpdatePlan,
) -> Result<GenerationEvidence, PlanExecutionError> {
    let selector_target = std::fs::read_link(&plan.auth_wrapper).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to read auth generation selector {}: {error}",
            plan.auth_wrapper.display()
        ))
    })?;
    let generation_dir =
        validate_generation_target_shape(&plan.auth_wrapper, &selector_target, plan)?;
    let generation_id = generation_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| PlanExecutionError::Failed("generation identity was not UTF-8".to_owned()))?
        .to_owned();
    let manifest = collect_generation_member(&generation_dir.join("generation.manifest"), 0o600)?;
    let manifest_values = parse_generation_manifest(&manifest.path)?;
    let helper =
        collect_generation_member(&generation_dir.join("shipyard-github-app-token"), 0o700)?;
    let wrapper = collect_generation_member(&generation_dir.join("ghapp"), 0o700)?;
    let binary = collect_generation_member(&generation_dir.join("shipyard"), 0o700)?;
    let companion = plan
        .companion_required
        .then(|| collect_generation_member(&generation_dir.join(COMPANION_BINARY_NAME), 0o700))
        .transpose()?;
    let context = tag_supports_auth_resolver(&plan.target)
        .then(|| {
            collect_generation_member(&generation_dir.join("ghapp.shipyard-context.json"), 0o600)
        })
        .transpose()?;
    validate_generation_manifest_values(
        &manifest_values,
        &generation_id,
        &helper,
        &wrapper,
        &binary,
        companion.as_ref(),
        context.as_ref(),
    )?;
    let authority_identity = manifest_values["authority_identity"].clone();
    let generation_contract = manifest_values["generation_contract"].clone();
    for member in [
        Some(&manifest),
        Some(&helper),
        Some(&wrapper),
        Some(&binary),
        companion.as_ref(),
        context.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        verify_generation_member_unchanged(member)?;
    }
    if let Some(context) = &context {
        validate_generation_context(context, &generation_id, &authority_identity, plan)?;
    }
    let selector_recheck_target = std::fs::read_link(&plan.auth_wrapper).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to re-read auth generation selector {}: {error}",
            plan.auth_wrapper.display()
        ))
    })?;
    if selector_recheck_target != selector_target {
        return Err(PlanExecutionError::Failed(
            "auth generation selector changed while evidence was collected".to_owned(),
        ));
    }
    Ok(GenerationEvidence {
        generation_contract,
        generation_id,
        authority_identity,
        selector_path: plan.auth_wrapper.clone(),
        selector_target,
        selector_recheck_target,
        manifest,
        helper,
        wrapper,
        binary,
        companion,
        context,
    })
}

fn verify_generation_member_unchanged(
    member: &GenerationMemberEvidence,
) -> Result<(), PlanExecutionError> {
    let recheck = sha256_file(&member.path).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to re-hash generation member {}: {error}",
            member.path.display()
        ))
    })?;
    if recheck != member.sha256 {
        return Err(PlanExecutionError::Failed(format!(
            "generation member {} changed while evidence was collected",
            member.path.display()
        )));
    }
    Ok(())
}

fn validate_generation_context(
    context: &GenerationMemberEvidence,
    generation_id: &str,
    authority_identity: &str,
    plan: &HostUpdatePlan,
) -> Result<(), PlanExecutionError> {
    let value: Value =
        serde_json::from_reader(std::fs::File::open(&context.path).map_err(|error| {
            PlanExecutionError::Failed(format!(
                "failed to open generation context {}: {error}",
                context.path.display()
            ))
        })?)
        .map_err(|_| {
            PlanExecutionError::Failed("generation context was not valid JSON".to_owned())
        })?;
    let object = value.as_object().ok_or_else(|| {
        PlanExecutionError::Failed("generation context was not a JSON object".to_owned())
    })?;
    let expected_keys = [
        "authority_identity",
        "generation_id",
        "global_dir",
        "mode",
        "schema_version",
    ];
    if object.len() != expected_keys.len()
        || expected_keys.iter().any(|key| !object.contains_key(*key))
        || object.get("schema_version").and_then(Value::as_u64) != Some(2)
        || object.get("generation_id").and_then(Value::as_str) != Some(generation_id)
        || object.get("authority_identity").and_then(Value::as_str) != Some(authority_identity)
        || object.get("global_dir").and_then(Value::as_str) != plan.global_dir.to_str()
        || object.get("mode").and_then(Value::as_str) != Some(plan.runtime_mode.as_str())
    {
        return Err(PlanExecutionError::Failed(
            "generation context did not bind its generation, authority, mode, and global directory"
                .to_owned(),
        ));
    }
    Ok(())
}

fn collect_generation_member(
    path: &Path,
    expected_mode: u32,
) -> Result<GenerationMemberEvidence, PlanExecutionError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to inspect generation member {}: {error}",
            path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != nix::unistd::Uid::effective().as_raw()
            || file_mode(&metadata) != expected_mode
        {
            return Err(PlanExecutionError::Failed(format!(
                "generation member {} was not an owned mode-{expected_mode:o} regular file",
                path.display()
            )));
        }
    }
    Ok(GenerationMemberEvidence {
        path: path.to_owned(),
        sha256: sha256_file(path).map_err(|error| {
            PlanExecutionError::Failed(format!(
                "failed to hash generation member {}: {error}",
                path.display()
            ))
        })?,
        mode: expected_mode,
    })
}

fn parse_generation_manifest(
    path: &Path,
) -> Result<std::collections::BTreeMap<String, String>, PlanExecutionError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to read generation manifest {}: {error}",
            path.display()
        ))
    })?;
    let mut values = std::collections::BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line.split_once('=').ok_or_else(|| {
            PlanExecutionError::Failed("generation manifest contained a malformed line".to_owned())
        })?;
        if key.is_empty()
            || value.is_empty()
            || values.insert(key.to_owned(), value.to_owned()).is_some()
        {
            return Err(PlanExecutionError::Failed(
                "generation manifest contained an empty or duplicate field".to_owned(),
            ));
        }
    }
    Ok(values)
}

fn validate_generation_manifest_values(
    values: &std::collections::BTreeMap<String, String>,
    generation_id: &str,
    helper: &GenerationMemberEvidence,
    wrapper: &GenerationMemberEvidence,
    binary: &GenerationMemberEvidence,
    companion: Option<&GenerationMemberEvidence>,
    context: Option<&GenerationMemberEvidence>,
) -> Result<(), PlanExecutionError> {
    let expected = std::collections::BTreeMap::from([
        ("schema_version".to_owned(), "1".to_owned()),
        (
            "generation_contract".to_owned(),
            "auth-selector-v1".to_owned(),
        ),
        ("generation_id".to_owned(), generation_id.to_owned()),
        (
            "authority_identity".to_owned(),
            values
                .get("authority_identity")
                .cloned()
                .unwrap_or_default(),
        ),
        ("helper_sha256".to_owned(), helper.sha256.clone()),
        ("helper_mode".to_owned(), format!("{:o}", helper.mode)),
        ("wrapper_sha256".to_owned(), wrapper.sha256.clone()),
        ("wrapper_mode".to_owned(), format!("{:o}", wrapper.mode)),
        ("binary_sha256".to_owned(), binary.sha256.clone()),
        ("binary_mode".to_owned(), format!("{:o}", binary.mode)),
        (
            "companion_sha256".to_owned(),
            companion.map_or_else(|| "absent".to_owned(), |member| member.sha256.clone()),
        ),
        (
            "context_sha256".to_owned(),
            context.map_or_else(|| "absent".to_owned(), |member| member.sha256.clone()),
        ),
        (
            "context_template_sha256".to_owned(),
            values
                .get("context_template_sha256")
                .cloned()
                .unwrap_or_default(),
        ),
    ]);
    if values != &expected
        || !valid_sha256(generation_id)
        || !values
            .get("authority_identity")
            .is_some_and(|value| valid_sha256(value))
        || !values
            .get("context_template_sha256")
            .is_some_and(|value| valid_sha256(value))
    {
        return Err(PlanExecutionError::Failed(
            "generation manifest did not bind the exact immutable member set".to_owned(),
        ));
    }
    Ok(())
}

fn collect_local_support_file(
    path: &Path,
    blob_oid: Option<&str>,
    verified: bool,
    plan: &HostUpdatePlan,
) -> Result<SupportFileEvidence, PlanExecutionError> {
    if !path_present_no_follow(path)? {
        return Ok(SupportFileEvidence {
            path: path.to_owned(),
            generation_target: None,
            sha256: None,
            mode: None,
            source_blob_oid: None,
            source_identity: None,
            source_identity_basis: SourceIdentityBasis::UnverifiedPreinstall,
        });
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to inspect support file {}: {error}",
            path.display()
        ))
    })?;
    let generation_target = if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(|error| {
            PlanExecutionError::Failed(format!(
                "failed to read support-file generation link {}: {error}",
                path.display()
            ))
        })?;
        validate_local_generation_target(path, &target, plan)?;
        Some(target)
    } else if metadata.file_type().is_file() {
        None
    } else {
        return Err(PlanExecutionError::Failed(format!(
            "support file {} was neither a regular file nor a generation link",
            path.display()
        )));
    };
    let followed_metadata = std::fs::metadata(path).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to inspect support-file generation target {}: {error}",
            path.display()
        ))
    })?;
    if !followed_metadata.file_type().is_file() {
        return Err(PlanExecutionError::Failed(format!(
            "support file {} did not resolve to a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    let mode = Some(file_mode(&followed_metadata));
    #[cfg(not(unix))]
    let mode = None;
    Ok(SupportFileEvidence {
        path: path.to_owned(),
        generation_target,
        sha256: Some(sha256_file(path).map_err(|error| {
            PlanExecutionError::Failed(format!(
                "failed to hash support file {}: {error}",
                path.display()
            ))
        })?),
        mode,
        source_blob_oid: blob_oid.map(str::to_owned),
        source_identity: verified.then(|| plan.source_identity.clone()),
        source_identity_basis: if verified {
            SourceIdentityBasis::VerifiedReleaseAuthority
        } else {
            SourceIdentityBasis::UnverifiedPreinstall
        },
    })
}

fn validate_local_generation_target(
    path: &Path,
    target: &Path,
    plan: &HostUpdatePlan,
) -> Result<(), PlanExecutionError> {
    let generation_root = generation_root(plan)?;
    let private_root = generation_root.parent().ok_or_else(|| {
        PlanExecutionError::Failed("generation root had no private parent".to_owned())
    })?;
    let share_root = private_root.parent().ok_or_else(|| {
        PlanExecutionError::Failed("generation private root had no share parent".to_owned())
    })?;
    let local_root = share_root.parent().ok_or_else(|| {
        PlanExecutionError::Failed("generation share root had no local parent".to_owned())
    })?;
    let expected_home = plan
        .auth_wrapper
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| {
            PlanExecutionError::Failed(
                "canonical auth wrapper did not identify its HOME root".to_owned(),
            )
        })?;
    if local_root.parent() != Some(expected_home)
        || local_root.file_name().and_then(|value| value.to_str()) != Some(".local")
        || share_root.file_name().and_then(|value| value.to_str()) != Some("share")
        || private_root.file_name().and_then(|value| value.to_str()) != Some("shipyard")
    {
        return Err(PlanExecutionError::Failed(
            "generation roots were not under the canonical HOME/.local/share/shipyard path"
                .to_owned(),
        ));
    }
    for directory in [expected_home, local_root, share_root] {
        validate_generation_parent(directory, false)?;
    }
    for directory in [private_root, generation_root.as_path()] {
        validate_generation_parent(directory, true)?;
    }
    let generation_dir = validate_generation_target_shape(path, target, plan)?;
    validate_generation_directory(&generation_dir)?;
    validate_generation_member(target)?;
    Ok(())
}

fn validate_generation_parent(directory: &Path, private: bool) -> Result<(), PlanExecutionError> {
    let metadata = std::fs::symlink_metadata(directory).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to inspect generation parent {}: {error}",
            directory.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = file_mode(&metadata);
        let unsafe_mode = if private {
            mode != 0o700
        } else {
            mode & 0o022 != 0
        };
        if !metadata.file_type().is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != nix::unistd::Uid::effective().as_raw()
            || unsafe_mode
        {
            let requirement = if private {
                "owned mode-0700 no-follow directory"
            } else {
                "owned no-follow non-writable directory"
            };
            return Err(PlanExecutionError::Failed(format!(
                "generation parent {} was not an {requirement}",
                directory.display()
            )));
        }
    }
    Ok(())
}

fn validate_generation_directory(generation_dir: &Path) -> Result<(), PlanExecutionError> {
    let generation_metadata = std::fs::symlink_metadata(generation_dir).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to inspect generation directory {}: {error}",
            generation_dir.display()
        ))
    })?;
    if !generation_metadata.file_type().is_dir() || generation_metadata.file_type().is_symlink() {
        return Err(PlanExecutionError::Failed(format!(
            "generation directory {} was not a no-follow directory",
            generation_dir.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if generation_metadata.uid() != nix::unistd::Uid::effective().as_raw()
            || file_mode(&generation_metadata) != 0o700
        {
            return Err(PlanExecutionError::Failed(format!(
                "generation directory {} had unsafe ownership or mode",
                generation_dir.display()
            )));
        }
    }
    Ok(())
}

fn validate_generation_member(target: &Path) -> Result<(), PlanExecutionError> {
    let target_metadata = std::fs::symlink_metadata(target).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to inspect generation target {}: {error}",
            target.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !target_metadata.file_type().is_file()
            || target_metadata.file_type().is_symlink()
            || target_metadata.uid() != nix::unistd::Uid::effective().as_raw()
            || file_mode(&target_metadata) != 0o700
        {
            return Err(PlanExecutionError::Failed(format!(
                "generation target {} was not an owned mode-0700 regular file",
                target.display()
            )));
        }
    }
    Ok(())
}

fn validate_generation_target_shape(
    path: &Path,
    target: &Path,
    plan: &HostUpdatePlan,
) -> Result<PathBuf, PlanExecutionError> {
    let root = generation_root(plan)?;
    if !root.is_absolute() || !target.is_absolute() {
        return Err(PlanExecutionError::Failed(format!(
            "support file {} used a non-absolute generation target",
            path.display()
        )));
    }
    let relative = target.strip_prefix(&root).map_err(|_| {
        PlanExecutionError::Failed(format!(
            "support file {} targeted a path outside the generation root",
            path.display()
        ))
    })?;
    let components = relative.components().collect::<Vec<_>>();
    let generation = components
        .first()
        .and_then(|component| component.as_os_str().to_str());
    let expected_name = path.file_name().ok_or_else(|| {
        PlanExecutionError::Failed(format!(
            "support path {} had no canonical file name",
            path.display()
        ))
    })?;
    if components.len() != 2
        || generation.is_none_or(|value| {
            value.len() != 64
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        || components.get(1).map(|component| component.as_os_str()) != Some(expected_name)
    {
        return Err(PlanExecutionError::Failed(format!(
            "support file {} used a malformed generation target",
            path.display()
        )));
    }
    let generation_dir = target.parent().ok_or_else(|| {
        PlanExecutionError::Failed(format!(
            "support file {} used a parentless generation target",
            path.display()
        ))
    })?;
    let expected_target = root
        .join(generation.ok_or_else(|| {
            PlanExecutionError::Failed(format!(
                "support file {} omitted its generation identity",
                path.display()
            ))
        })?)
        .join(expected_name);
    if expected_target != target {
        return Err(PlanExecutionError::Failed(format!(
            "support file {} used a non-canonical generation target",
            path.display()
        )));
    }
    Ok(generation_dir.to_owned())
}

fn generation_root(plan: &HostUpdatePlan) -> Result<PathBuf, PlanExecutionError> {
    let home = plan
        .binary
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| PlanExecutionError::Failed("fleet binary has no home root".to_owned()))?;
    Ok(home.join(".local/share/shipyard/auth-generations"))
}

#[cfg(unix)]
fn file_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

fn collect_local_pair(
    plan: &HostUpdatePlan,
    host_deadline: Instant,
    verified_installer_target: bool,
) -> Result<BinaryPairEvidence, PlanExecutionError> {
    let primary = collect_local_binary(
        &plan.binary,
        "shipyard",
        plan,
        host_deadline,
        verified_installer_target,
    )?;
    let companion = if path_present_no_follow(&plan.companion_binary)? {
        Some(collect_local_binary(
            &plan.companion_binary,
            COMPANION_BINARY_NAME,
            plan,
            host_deadline,
            verified_installer_target,
        )?)
    } else {
        None
    };
    Ok(BinaryPairEvidence { primary, companion })
}

fn path_present_no_follow(path: &Path) -> Result<bool, PlanExecutionError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(PlanExecutionError::Failed(format!(
            "failed to inspect binary {}: {error}",
            path.display()
        ))),
    }
}

fn collect_local_binary(
    path: &Path,
    label: &str,
    plan: &HostUpdatePlan,
    host_deadline: Instant,
    verified_installer_target: bool,
) -> Result<BinaryEvidence, PlanExecutionError> {
    ensure_before_deadline(host_deadline, "installed executable hash")?;
    let sha256_before = sha256_file(path).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to hash installed binary {}: {error}",
            path.display()
        ))
    })?;
    let semantic_version = command_version(path, label, plan, host_deadline)?;
    ensure_before_deadline(host_deadline, "installed executable hash")?;
    let sha256 = sha256_file(path).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to hash installed binary {}: {error}",
            path.display()
        ))
    })?;
    ensure_before_deadline(host_deadline, "installed executable hash")?;
    if sha256_before != sha256 {
        return Err(PlanExecutionError::Failed(format!(
            "installed binary {} changed during identity observation",
            path.display()
        )));
    }
    Ok(BinaryEvidence {
        path: path.to_owned(),
        source_identity: verified_installer_target.then(|| plan.source_identity.clone()),
        source_identity_basis: if verified_installer_target {
            SourceIdentityBasis::VerifiedReleaseAuthority
        } else {
            SourceIdentityBasis::UnverifiedPreinstall
        },
        semantic_version,
        sha256,
    })
}

pub(super) fn parse_remote_evidence(
    plan: &HostUpdatePlan,
    stdout: &[u8],
) -> Result<HostUpdateEvidence, PlanExecutionError> {
    let text = std::str::from_utf8(stdout).map_err(|_| {
        PlanExecutionError::Failed("remote fleet evidence was not UTF-8".to_owned())
    })?;
    let before_pair = remote_pair_from_markers(plan, text, false)?;
    validate_binary_pair(plan, &before_pair, None).map_err(PlanExecutionError::Failed)?;
    let after_pair = remote_pair_from_markers(plan, text, true)?;
    let before_auth = remote_auth_support_from_markers(plan, text, false)?;
    let after_auth = remote_auth_support_from_markers(plan, text, true)?;
    let before_status = parse_json_text(
        &unique_marker(text, REMOTE_BEFORE_STATUS_PREFIX)?,
        "remote pre-update daemon status",
    )?;
    let refresh = parse_json_text(
        &unique_marker(text, REMOTE_REFRESH_PREFIX)?,
        "remote daemon refresh receipt",
    )?;
    if refresh.get("command").and_then(Value::as_str) != Some("daemon:refresh") {
        return Err(PlanExecutionError::Failed(
            "remote daemon refresh returned an unexpected typed receipt".to_owned(),
        ));
    }
    let daemon_pid = refresh
        .get("new_pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .ok_or_else(|| {
            PlanExecutionError::Failed(
                "remote daemon refresh returned no nonzero new_pid".to_owned(),
            )
        })?;
    let after_status = parse_json_text(
        &unique_marker(text, REMOTE_AFTER_STATUS_PREFIX)?,
        "remote post-update daemon status",
    )?;
    let daemon_runtime =
        daemon_runtime_from_markers(plan, text, daemon_pid, &after_status, &after_auth)?;
    let release_authority_identity = unique_marker(text, REMOTE_AUTHORITY_ID_PREFIX)?;
    let release_asset_sha256 = unique_marker(text, REMOTE_RELEASE_ASSET_SHA256_PREFIX)?;
    let evidence = evidence_from_values(
        before_pair,
        after_pair,
        before_auth,
        after_auth,
        daemon_pid,
        daemon_runtime,
        &before_status,
        &after_status,
        release_authority_identity,
        release_asset_sha256,
    )?;
    if plan.ssh.is_none() {
        return Err(PlanExecutionError::Failed(
            "remote evidence was returned for a local plan".to_owned(),
        ));
    }
    Ok(evidence)
}

fn daemon_runtime_from_markers(
    plan: &HostUpdatePlan,
    text: &str,
    daemon_pid: u32,
    after_status: &Value,
    after_auth: &AuthSupportEvidence,
) -> Result<DaemonRuntimeEvidence, PlanExecutionError> {
    let observed_pid = unique_marker(text, REMOTE_DAEMON_PID_PREFIX)?
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| {
            PlanExecutionError::Failed("remote daemon PID marker was invalid".to_owned())
        })?;
    let generation = after_auth.generation.as_ref().ok_or_else(|| {
        PlanExecutionError::Failed("remote daemon receipt omitted auth generation".to_owned())
    })?;
    let loaded_executable_path =
        PathBuf::from(unique_marker(text, REMOTE_DAEMON_EXECUTABLE_PREFIX)?);
    let loaded_executable_sha256 = unique_marker(text, REMOTE_DAEMON_EXECUTABLE_SHA_PREFIX)?;
    let loaded_launch = unique_marker(text, REMOTE_DAEMON_LAUNCH_PREFIX)?;
    let machine_auth_probe_sha256 = unique_marker(text, REMOTE_DAEMON_AUTH_PROBE_SHA_PREFIX)?;
    let rendered_launch = rendered_daemon_launch(plan, &configured_repos(after_status)?);
    if observed_pid != daemon_pid
        || loaded_executable_path != generation.binary.path
        || !valid_sha256(&loaded_executable_sha256)
        || !valid_sha256(&machine_auth_probe_sha256)
    {
        return Err(PlanExecutionError::Failed(
            "remote daemon runtime markers did not bind the refreshed generation".to_owned(),
        ));
    }
    Ok(DaemonRuntimeEvidence {
        pid: observed_pid,
        loaded_executable_path,
        loaded_executable_sha256,
        rendered_launch_sha256: sha256_bytes(rendered_launch.as_bytes()),
        loaded_launch_sha256: sha256_bytes(loaded_launch.as_bytes()),
        machine_auth_probe_sha256,
        machine_auth_generation_id: generation.generation_id.clone(),
    })
}

fn remote_auth_support_from_markers(
    plan: &HostUpdatePlan,
    text: &str,
    after: bool,
) -> Result<AuthSupportEvidence, PlanExecutionError> {
    let (helper_sha, helper_mode, helper_target, wrapper_sha, wrapper_mode, wrapper_target) =
        if after {
            (
                auth_support::AFTER_HELPER_SHA_PREFIX,
                auth_support::AFTER_HELPER_MODE_PREFIX,
                auth_support::AFTER_HELPER_TARGET_PREFIX,
                auth_support::AFTER_WRAPPER_SHA_PREFIX,
                auth_support::AFTER_WRAPPER_MODE_PREFIX,
                auth_support::AFTER_WRAPPER_TARGET_PREFIX,
            )
        } else {
            (
                auth_support::BEFORE_HELPER_SHA_PREFIX,
                auth_support::BEFORE_HELPER_MODE_PREFIX,
                auth_support::BEFORE_HELPER_TARGET_PREFIX,
                auth_support::BEFORE_WRAPPER_SHA_PREFIX,
                auth_support::BEFORE_WRAPPER_MODE_PREFIX,
                auth_support::BEFORE_WRAPPER_TARGET_PREFIX,
            )
        };
    Ok(AuthSupportEvidence {
        helper: support_file_from_markers(
            &plan.auth_helper,
            &unique_marker(text, helper_sha)?,
            &unique_marker(text, helper_mode)?,
            &unique_marker(text, helper_target)?,
            after.then_some(plan.release_authority.auth_helper.blob_oid.as_str()),
            after,
            plan,
        )?,
        wrapper: support_file_from_markers(
            &plan.auth_wrapper,
            &unique_marker(text, wrapper_sha)?,
            &unique_marker(text, wrapper_mode)?,
            &unique_marker(text, wrapper_target)?,
            after.then_some(plan.release_authority.auth_wrapper.blob_oid.as_str()),
            after,
            plan,
        )?,
        generation: after
            .then(|| generation_from_markers(plan, text))
            .transpose()?,
    })
}

fn generation_from_markers(
    plan: &HostUpdatePlan,
    text: &str,
) -> Result<GenerationEvidence, PlanExecutionError> {
    let selector_target = PathBuf::from(unique_marker(text, REMOTE_GENERATION_SELECTOR_PREFIX)?);
    let selector_recheck_target = PathBuf::from(unique_marker(
        text,
        REMOTE_GENERATION_SELECTOR_RECHECK_PREFIX,
    )?);
    if selector_target != selector_recheck_target {
        return Err(PlanExecutionError::Failed(
            "remote auth generation selector changed while evidence was collected".to_owned(),
        ));
    }
    let generation_dir =
        validate_generation_target_shape(&plan.auth_wrapper, &selector_target, plan)?;
    let generation_id = unique_marker(text, REMOTE_GENERATION_ID_PREFIX)?;
    if generation_dir.file_name().and_then(|value| value.to_str()) != Some(&generation_id)
        || !valid_sha256(&generation_id)
    {
        return Err(PlanExecutionError::Failed(
            "remote auth generation identity disagreed with the selector".to_owned(),
        ));
    }
    let authority_identity = unique_marker(text, REMOTE_GENERATION_AUTHORITY_PREFIX)?;
    if !valid_sha256(&authority_identity) {
        return Err(PlanExecutionError::Failed(
            "remote auth generation authority identity was invalid".to_owned(),
        ));
    }
    let member = |name: &str, prefix: &str, mode: u32| {
        let sha256 = unique_marker(text, prefix)?;
        if !valid_sha256(&sha256) {
            return Err(PlanExecutionError::Failed(format!(
                "remote auth generation member {name} had an invalid digest"
            )));
        }
        Ok(GenerationMemberEvidence {
            path: generation_dir.join(name),
            sha256,
            mode,
        })
    };
    let optional_member = |name: &str, prefix: &str, mode: u32, required: bool| {
        let sha256 = unique_marker(text, prefix)?;
        match (sha256.as_str(), required) {
            ("absent", false) => Ok(None),
            ("absent", true) => Err(PlanExecutionError::Failed(format!(
                "remote auth generation omitted required member {name}"
            ))),
            (_, false) => Err(PlanExecutionError::Failed(format!(
                "remote auth generation retained unexpected member {name}"
            ))),
            (_, true) if valid_sha256(&sha256) => Ok(Some(GenerationMemberEvidence {
                path: generation_dir.join(name),
                sha256,
                mode,
            })),
            _ => Err(PlanExecutionError::Failed(format!(
                "remote auth generation member {name} had an invalid digest"
            ))),
        }
    };
    let generation_contract = unique_marker(text, REMOTE_GENERATION_CONTRACT_PREFIX)?;
    if generation_contract != "auth-selector-v1" {
        return Err(PlanExecutionError::Failed(
            "remote auth generation contract was unsupported".to_owned(),
        ));
    }
    Ok(GenerationEvidence {
        generation_contract,
        generation_id,
        authority_identity,
        selector_path: plan.auth_wrapper.clone(),
        selector_target,
        selector_recheck_target,
        manifest: member(
            "generation.manifest",
            REMOTE_GENERATION_MANIFEST_SHA_PREFIX,
            0o600,
        )?,
        helper: member(
            "shipyard-github-app-token",
            REMOTE_GENERATION_HELPER_SHA_PREFIX,
            0o700,
        )?,
        wrapper: member("ghapp", REMOTE_GENERATION_WRAPPER_SHA_PREFIX, 0o700)?,
        binary: member("shipyard", REMOTE_GENERATION_BINARY_SHA_PREFIX, 0o700)?,
        companion: optional_member(
            COMPANION_BINARY_NAME,
            REMOTE_GENERATION_COMPANION_SHA_PREFIX,
            0o700,
            plan.companion_required,
        )?,
        context: optional_member(
            "ghapp.shipyard-context.json",
            REMOTE_GENERATION_CONTEXT_SHA_PREFIX,
            0o600,
            tag_supports_auth_resolver(&plan.target),
        )?,
    })
}

fn support_file_from_markers(
    path: &Path,
    sha256: &str,
    mode: &str,
    target: &str,
    blob_oid: Option<&str>,
    verified: bool,
    plan: &HostUpdatePlan,
) -> Result<SupportFileEvidence, PlanExecutionError> {
    let (sha256, mode, generation_target) = match (sha256, mode, target) {
        ("absent", "absent", "absent") => (None, None, None),
        ("absent", _, _) | (_, "absent", _) | (_, _, "absent") => {
            return Err(PlanExecutionError::Failed(
                "auth support presence markers disagreed".to_owned(),
            ));
        }
        (sha256, mode, target) => {
            if !valid_sha256(sha256) {
                return Err(PlanExecutionError::Failed(
                    "auth support SHA-256 marker was invalid".to_owned(),
                ));
            }
            let parsed = u32::from_str_radix(mode, 8).map_err(|_| {
                PlanExecutionError::Failed("auth support mode marker was invalid".to_owned())
            })?;
            let generation_target = match target {
                "direct" => None,
                value => {
                    let target = PathBuf::from(value);
                    validate_generation_target_shape(path, &target, plan)?;
                    Some(target)
                }
            };
            (Some(sha256.to_owned()), Some(parsed), generation_target)
        }
    };
    Ok(SupportFileEvidence {
        path: path.to_owned(),
        generation_target,
        sha256,
        mode,
        source_blob_oid: blob_oid.map(str::to_owned),
        source_identity: verified.then(|| plan.source_identity.clone()),
        source_identity_basis: if verified {
            SourceIdentityBasis::VerifiedReleaseAuthority
        } else {
            SourceIdentityBasis::UnverifiedPreinstall
        },
    })
}

fn remote_pair_from_markers(
    plan: &HostUpdatePlan,
    text: &str,
    after: bool,
) -> Result<BinaryPairEvidence, PlanExecutionError> {
    let (
        primary_sha_prefix,
        primary_version_prefix,
        companion_sha_prefix,
        companion_version_prefix,
    ) = if after {
        (
            REMOTE_AFTER_PRIMARY_SHA256_PREFIX,
            REMOTE_AFTER_PRIMARY_VERSION_PREFIX,
            REMOTE_AFTER_COMPANION_SHA256_PREFIX,
            REMOTE_AFTER_COMPANION_VERSION_PREFIX,
        )
    } else {
        (
            REMOTE_BEFORE_PRIMARY_SHA256_PREFIX,
            REMOTE_BEFORE_PRIMARY_VERSION_PREFIX,
            REMOTE_BEFORE_COMPANION_SHA256_PREFIX,
            REMOTE_BEFORE_COMPANION_VERSION_PREFIX,
        )
    };
    let primary_version =
        parse_version_output(&unique_marker(text, primary_version_prefix)?, "shipyard")?;
    let primary = BinaryEvidence {
        path: plan.binary.clone(),
        sha256: unique_marker(text, primary_sha_prefix)?,
        source_identity: after.then(|| plan.source_identity.clone()),
        source_identity_basis: if after {
            SourceIdentityBasis::VerifiedReleaseAuthority
        } else {
            SourceIdentityBasis::UnverifiedPreinstall
        },
        semantic_version: primary_version,
    };
    let companion_sha = unique_marker(text, companion_sha_prefix)?;
    let companion_version = unique_marker(text, companion_version_prefix)?;
    let companion = match (companion_sha.as_str(), companion_version.as_str()) {
        ("absent", "absent") => None,
        ("absent", _) | (_, "absent") => {
            return Err(PlanExecutionError::Failed(
                "companion hash/version presence markers disagreed".to_owned(),
            ));
        }
        _ => {
            let semantic_version = parse_version_output(&companion_version, COMPANION_BINARY_NAME)?;
            Some(BinaryEvidence {
                path: plan.companion_binary.clone(),
                sha256: companion_sha,
                source_identity: after.then(|| plan.source_identity.clone()),
                source_identity_basis: if after {
                    SourceIdentityBasis::VerifiedReleaseAuthority
                } else {
                    SourceIdentityBasis::UnverifiedPreinstall
                },
                semantic_version,
            })
        }
    };
    Ok(BinaryPairEvidence { primary, companion })
}

fn unique_marker(text: &str, prefix: &str) -> Result<String, PlanExecutionError> {
    let values = text
        .lines()
        .filter_map(|line| line.strip_prefix(prefix))
        .collect::<Vec<_>>();
    if values.len() != 1 || values[0].is_empty() {
        return Err(PlanExecutionError::Failed(format!(
            "fleet evidence marker {prefix:?} must appear exactly once"
        )));
    }
    Ok(values[0].to_owned())
}

fn parse_json_value(bytes: &[u8], label: &str) -> Result<Value, PlanExecutionError> {
    serde_json::from_slice(bytes).map_err(|_| {
        PlanExecutionError::Failed(format!("{label} returned an invalid JSON receipt"))
    })
}

fn parse_json_text(text: &str, label: &str) -> Result<Value, PlanExecutionError> {
    serde_json::from_str(text).map_err(|_| {
        PlanExecutionError::Failed(format!("{label} returned an invalid JSON receipt"))
    })
}

fn command_version(
    binary: &Path,
    label: &str,
    plan: &HostUpdatePlan,
    host_deadline: Instant,
) -> Result<String, PlanExecutionError> {
    let mut command = Command::new(binary);
    command
        .arg("--version")
        .env_clear()
        .env("HOME", home_dir())
        .env("PATH", unattended_tool_path());
    let output = run_bounded_output(
        &mut command,
        probe_deadline(host_deadline),
        &format!("installed CLI version probe for host class {}", plan.class),
    )?;
    if !output.status.success() {
        return Err(PlanExecutionError::Failed(format!(
            "installed binary --version exited {}",
            output.status.code().unwrap_or(-1)
        )));
    }
    let output = String::from_utf8(output.stdout).map_err(|_| {
        PlanExecutionError::Failed("installed binary version was not UTF-8".to_owned())
    })?;
    parse_version_output(&output, label)
}

fn parse_version_output(output: &str, label: &str) -> Result<String, PlanExecutionError> {
    let expected_prefix = format!("{label} ");
    let version = output
        .strip_suffix('\n')
        .unwrap_or(output)
        .strip_prefix(&expected_prefix)
        .filter(|version| !version.is_empty() && !version.contains(['\r', '\n']))
        .ok_or_else(|| {
            PlanExecutionError::Failed(format!(
                "installed {label} version did not match the exact '<name> <version>' contract"
            ))
        })?;
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
                || part.parse::<u64>().is_err()
        })
    {
        return Err(PlanExecutionError::Failed(format!(
            "installed {label} version was invalid"
        )));
    }
    Ok(version.to_owned())
}

fn probe_deadline(host_deadline: Instant) -> Instant {
    host_deadline.min(Instant::now() + Duration::from_secs(15))
}

fn ensure_before_deadline(
    host_deadline: Instant,
    operation: &str,
) -> Result<(), PlanExecutionError> {
    if Instant::now() < host_deadline {
        return Ok(());
    }
    Err(PlanExecutionError::TimedOut(format!(
        "{operation} exhausted the bounded host-attempt deadline"
    )))
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[allow(clippy::too_many_arguments)] // One typed join of paired binary, support, daemon, and authority observations.
fn evidence_from_values(
    before_pair: BinaryPairEvidence,
    after_pair: BinaryPairEvidence,
    auth_support_before: AuthSupportEvidence,
    auth_support_after: AuthSupportEvidence,
    daemon_pid: u32,
    daemon_runtime: DaemonRuntimeEvidence,
    before_status: &Value,
    after_status: &Value,
    release_authority_identity: String,
    release_asset_sha256: String,
) -> Result<HostUpdateEvidence, PlanExecutionError> {
    if after_status.get("command").and_then(Value::as_str) != Some("daemon:status")
        || after_status.get("running").and_then(Value::as_bool) != Some(true)
    {
        return Err(PlanExecutionError::Failed(
            "post-update daemon status did not prove a running daemon".to_owned(),
        ));
    }
    let daemon_version = after_status
        .get("shipyard_version")
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty())
        .ok_or_else(|| {
            PlanExecutionError::Failed(
                "post-update daemon status omitted shipyard_version".to_owned(),
            )
        })?
        .to_owned();
    let configured_repos_after = configured_repos(after_status)?;
    let configured_repos_before = match before_status.get("running").and_then(Value::as_bool) {
        Some(true) => Some(configured_repos(before_status)?),
        Some(false) => None,
        None => {
            return Err(PlanExecutionError::Failed(
                "pre-update daemon status omitted running".to_owned(),
            ));
        }
    };
    let configured_repos_preserved = configured_repos_before.as_ref().map(|before| {
        let mut before = before.clone();
        let mut after = configured_repos_after.clone();
        before.sort();
        after.sort();
        before == after
    });
    Ok(HostUpdateEvidence {
        release_authority_identity,
        release_asset_sha256,
        executable_sha256: after_pair.primary.sha256.clone(),
        cli_version: format!("shipyard {}", after_pair.primary.semantic_version),
        before_pair,
        after_pair,
        auth_support_before,
        auth_support_after,
        daemon_version,
        daemon_pid,
        daemon_runtime,
        configured_repos_before,
        configured_repos_after,
        configured_repos_preserved,
    })
}

fn configured_repos(status: &Value) -> Result<Vec<String>, PlanExecutionError> {
    status
        .get("configured_repos")
        .or_else(|| status.get("registered_repos"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            PlanExecutionError::Failed("daemon status omitted configured_repos".to_owned())
        })?
        .iter()
        .map(|repo| {
            repo.as_str().map(str::to_owned).ok_or_else(|| {
                PlanExecutionError::Failed(
                    "daemon status configured_repos contained a non-string".to_owned(),
                )
            })
        })
        .collect()
}

pub(super) fn valid_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_binary_pair(
    plan: &HostUpdatePlan,
    pair: &BinaryPairEvidence,
    expected_tag: Option<&str>,
) -> Result<(), String> {
    if pair.primary.path != plan.binary || !valid_sha256(&pair.primary.sha256) {
        return Err("primary binary path or SHA-256 evidence was invalid".to_owned());
    }
    let observed_tag = format!("v{}", pair.primary.semantic_version);
    match expected_tag {
        None if pair.primary.source_identity.is_some()
            || pair.primary.source_identity_basis != SourceIdentityBasis::UnverifiedPreinstall =>
        {
            return Err("pre-install binary provenance was not explicitly unverified".to_owned());
        }
        Some(_)
            if pair.primary.source_identity.as_deref() != Some(plan.source_identity.as_str())
                || pair.primary.source_identity_basis
                    != SourceIdentityBasis::VerifiedReleaseAuthority =>
        {
            return Err(
                "post-install binary provenance was not bound to the verified installer target"
                    .to_owned(),
            );
        }
        _ => {}
    }
    let requires_companion = tag_requires_companion(&observed_tag);
    match (&pair.companion, requires_companion) {
        (Some(companion), true) => {
            if companion.path != plan.companion_binary || !valid_sha256(&companion.sha256) {
                return Err("companion binary path or SHA-256 evidence was invalid".to_owned());
            }
            if companion.semantic_version != pair.primary.semantic_version
                || companion.source_identity != pair.primary.source_identity
                || companion.source_identity_basis != pair.primary.source_identity_basis
            {
                return Err("primary and companion binary identities were mixed".to_owned());
            }
        }
        (None, true) => return Err("required companion binary was absent".to_owned()),
        (Some(_), false) => {
            return Err("legacy single-binary release retained a mixed companion".to_owned());
        }
        (None, false) => {}
    }
    if let Some(expected_tag) = expected_tag
        && (observed_tag != expected_tag || requires_companion != plan.companion_required)
    {
        return Err("installed binary pair did not match the planned release source".to_owned());
    }
    Ok(())
}

pub(super) fn validate_evidence(
    plan: &HostUpdatePlan,
    evidence: &HostUpdateEvidence,
) -> Result<(), String> {
    if evidence.release_authority_identity != plan.release_authority.identity_sha256
        || evidence.release_asset_sha256 != plan.release_authority.platform_asset.sha256
    {
        return Err(
            "host receipt was not bound to the frozen release authority and exact platform asset"
                .to_owned(),
        );
    }
    validate_binary_pair(plan, &evidence.before_pair, None)?;
    validate_binary_pair(plan, &evidence.after_pair, Some(&plan.target))?;
    validate_auth_support(plan, &evidence.auth_support_before, false)?;
    validate_auth_support(plan, &evidence.auth_support_after, true)?;
    let generation = evidence
        .auth_support_after
        .generation
        .as_ref()
        .ok_or_else(|| "post-install evidence omitted composed auth generation".to_owned())?;
    if generation.binary.sha256 != evidence.after_pair.primary.sha256
        || generation
            .companion
            .as_ref()
            .map(|member| member.sha256.as_str())
            != evidence
                .after_pair
                .companion
                .as_ref()
                .map(|member| member.sha256.as_str())
    {
        return Err("composed auth generation disagreed with installed binary pair".to_owned());
    }
    let version = plan.target.trim_start_matches('v');
    let expected_cli = format!("shipyard {version}");
    if evidence.cli_version != expected_cli
        || evidence.executable_sha256 != evidence.after_pair.primary.sha256
    {
        return Err(format!(
            "legacy primary evidence disagreed with the paired receipt: expected version {expected_cli:?}"
        ));
    }
    if evidence.daemon_version != version {
        return Err(format!(
            "daemon version mismatch: expected {version:?}, observed {:?}",
            evidence.daemon_version
        ));
    }
    if evidence.daemon_runtime.pid != evidence.daemon_pid
        || evidence.daemon_runtime.loaded_executable_path != generation.binary.path
        || evidence.daemon_runtime.loaded_executable_sha256 != generation.binary.sha256
        || evidence.daemon_runtime.rendered_launch_sha256
            != evidence.daemon_runtime.loaded_launch_sha256
        || evidence.daemon_runtime.machine_auth_generation_id != generation.generation_id
        || !valid_sha256(&evidence.daemon_runtime.machine_auth_probe_sha256)
    {
        return Err(
            "refreshed daemon was not bound to the exact loaded generation, launch identity, and machine-global auth selector"
                .to_owned(),
        );
    }
    if evidence.configured_repos_preserved == Some(false) {
        return Err("configured repositories changed across daemon refresh".to_owned());
    }
    Ok(())
}

fn validate_auth_support(
    plan: &HostUpdatePlan,
    support: &AuthSupportEvidence,
    after: bool,
) -> Result<(), String> {
    for (observed, path, authority) in [
        (
            &support.helper,
            &plan.auth_helper,
            &plan.release_authority.auth_helper,
        ),
        (
            &support.wrapper,
            &plan.auth_wrapper,
            &plan.release_authority.auth_wrapper,
        ),
    ] {
        if observed.path != *path {
            return Err("auth support evidence used an unexpected path".to_owned());
        }
        if after {
            if observed.sha256.as_deref() != Some(authority.sha256.as_str())
                || observed.mode != Some(0o700)
                || observed.source_blob_oid.as_deref() != Some(authority.blob_oid.as_str())
                || observed.source_identity.as_deref() != Some(plan.source_identity.as_str())
                || observed.source_identity_basis != SourceIdentityBasis::VerifiedReleaseAuthority
            {
                return Err(
                    "post-install auth support was mixed, tampered, unsafe, or not release-bound"
                        .to_owned(),
                );
            }
        } else if observed.source_blob_oid.is_some()
            || observed.source_identity.is_some()
            || observed.source_identity_basis != SourceIdentityBasis::UnverifiedPreinstall
            || observed.sha256.is_some() != observed.mode.is_some()
        {
            return Err(
                "pre-install auth support provenance was not explicitly unverified".to_owned(),
            );
        }
    }
    if after {
        let helper_target = support
            .helper
            .generation_target
            .as_deref()
            .ok_or_else(|| "post-install auth helper was not generation-bound".to_owned())?;
        let wrapper_target = support
            .wrapper
            .generation_target
            .as_deref()
            .ok_or_else(|| "post-install auth wrapper was not generation-bound".to_owned())?;
        validate_generation_target_shape(&support.helper.path, helper_target, plan)
            .map_err(plan_execution_message)?;
        validate_generation_target_shape(&support.wrapper.path, wrapper_target, plan)
            .map_err(plan_execution_message)?;
        if helper_target.parent() != wrapper_target.parent() {
            return Err("post-install auth support mixed generation identities".to_owned());
        }
        let generation = support.generation.as_ref().ok_or_else(|| {
            "post-install auth support omitted composed generation evidence".to_owned()
        })?;
        validate_generation_evidence(plan, support, generation)?;
    } else if support.generation.is_some() {
        return Err("pre-install auth support claimed verified generation evidence".to_owned());
    }
    Ok(())
}

fn validate_generation_evidence(
    plan: &HostUpdatePlan,
    support: &AuthSupportEvidence,
    generation: &GenerationEvidence,
) -> Result<(), String> {
    if generation.selector_path != plan.auth_wrapper
        || generation.generation_contract != "auth-selector-v1"
        || generation.selector_target != generation.selector_recheck_target
        || generation.authority_identity != plan.release_authority.identity_sha256
        || !valid_sha256(&generation.generation_id)
        || !valid_sha256(&generation.manifest.sha256)
    {
        return Err("composed auth generation identity was unstable or unauthorized".to_owned());
    }
    let generation_dir =
        validate_generation_target_shape(&plan.auth_wrapper, &generation.selector_target, plan)
            .map_err(plan_execution_message)?;
    if generation_dir.file_name().and_then(|value| value.to_str())
        != Some(generation.generation_id.as_str())
    {
        return Err("composed auth generation selector disagreed with its identity".to_owned());
    }
    let validate_member =
        |member: &GenerationMemberEvidence, name: &str, mode: u32| -> Result<(), String> {
            if member.path != generation_dir.join(name)
                || member.mode != mode
                || !valid_sha256(&member.sha256)
            {
                return Err(format!(
                    "composed auth generation member {name} was invalid"
                ));
            }
            Ok(())
        };
    validate_member(&generation.manifest, "generation.manifest", 0o600)?;
    validate_member(&generation.helper, "shipyard-github-app-token", 0o700)?;
    validate_member(&generation.wrapper, "ghapp", 0o700)?;
    validate_member(&generation.binary, "shipyard", 0o700)?;
    if generation.helper.sha256 != support.helper.sha256.as_deref().unwrap_or_default()
        || generation.wrapper.sha256 != support.wrapper.sha256.as_deref().unwrap_or_default()
    {
        return Err(
            "composed auth generation disagreed with installed support or asset authority"
                .to_owned(),
        );
    }
    match (&generation.companion, plan.companion_required) {
        (Some(companion), true) => {
            validate_member(companion, COMPANION_BINARY_NAME, 0o700)?;
        }
        (None, false) => {}
        _ => return Err("composed auth generation companion presence was invalid".to_owned()),
    }
    match (
        &generation.context,
        tag_supports_auth_resolver(&plan.target),
    ) {
        (Some(context), true) => {
            validate_member(context, "ghapp.shipyard-context.json", 0o600)?;
        }
        (None, false) => {}
        _ => return Err("composed auth generation context presence was invalid".to_owned()),
    }
    if support.helper.generation_target.as_deref() != Some(generation.helper.path.as_path())
        || support.wrapper.generation_target.as_deref() != Some(generation.wrapper.path.as_path())
    {
        return Err("compatibility projections disagreed with composed auth generation".to_owned());
    }
    Ok(())
}

fn plan_execution_message(error: PlanExecutionError) -> String {
    match error {
        PlanExecutionError::TimedOut(message) | PlanExecutionError::Failed(message) => message,
    }
}

fn ssh_binary() -> PathBuf {
    [PathBuf::from("/usr/bin/ssh"), PathBuf::from("/bin/ssh")]
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("ssh"))
}

#[cfg(test)]
mod tests;
