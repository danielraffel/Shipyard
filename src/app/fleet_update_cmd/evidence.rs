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
    REMOTE_RELEASE_ASSET_SHA256_PREFIX, tag_requires_companion,
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
    pub(super) configured_repos_before: Option<Vec<String>>,
    pub(super) configured_repos_after: Vec<String>,
    pub(super) configured_repos_preserved: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct AuthSupportEvidence {
    pub(super) helper: SupportFileEvidence,
    pub(super) wrapper: SupportFileEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SupportFileEvidence {
    pub(super) path: PathBuf,
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
    let daemon_pid = serde_json::Deserializer::from_slice(update_stdout)
        .into_iter::<Value>()
        .filter_map(Result::ok)
        .find_map(|value| {
            (value.get("event").and_then(Value::as_str) == Some("daemon_refreshed"))
                .then(|| value.get("daemon_pid").and_then(Value::as_u64))
                .flatten()
        })
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .ok_or_else(|| {
            PlanExecutionError::Failed(
                "local update returned no typed nonzero daemon PID receipt".to_owned(),
            )
        })?;
    evidence_from_values(
        before_pair,
        after_pair,
        before_auth,
        after_auth,
        daemon_pid,
        before_status,
        &after_status,
        plan.release_authority.identity_sha256.clone(),
        plan.release_authority.platform_asset.sha256.clone(),
    )
}

fn collect_local_auth_support(
    plan: &HostUpdatePlan,
    verified: bool,
) -> Result<AuthSupportEvidence, PlanExecutionError> {
    Ok(AuthSupportEvidence {
        helper: collect_local_support_file(
            &plan.auth_helper,
            verified.then_some(plan.release_authority.auth_helper.blob_oid.as_str()),
            verified,
            plan,
        )?,
        wrapper: collect_local_support_file(
            &plan.auth_wrapper,
            verified.then_some(plan.release_authority.auth_wrapper.blob_oid.as_str()),
            verified,
            plan,
        )?,
    })
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
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(PlanExecutionError::Failed(format!(
            "support file {} was not a no-follow regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    let mode = Some(file_mode(&metadata));
    #[cfg(not(unix))]
    let mode = None;
    Ok(SupportFileEvidence {
        path: path.to_owned(),
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
    let release_authority_identity = unique_marker(text, REMOTE_AUTHORITY_ID_PREFIX)?;
    let release_asset_sha256 = unique_marker(text, REMOTE_RELEASE_ASSET_SHA256_PREFIX)?;
    let evidence = evidence_from_values(
        before_pair,
        after_pair,
        before_auth,
        after_auth,
        daemon_pid,
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

fn remote_auth_support_from_markers(
    plan: &HostUpdatePlan,
    text: &str,
    after: bool,
) -> Result<AuthSupportEvidence, PlanExecutionError> {
    let (helper_sha, helper_mode, wrapper_sha, wrapper_mode) = if after {
        (
            auth_support::AFTER_HELPER_SHA_PREFIX,
            auth_support::AFTER_HELPER_MODE_PREFIX,
            auth_support::AFTER_WRAPPER_SHA_PREFIX,
            auth_support::AFTER_WRAPPER_MODE_PREFIX,
        )
    } else {
        (
            auth_support::BEFORE_HELPER_SHA_PREFIX,
            auth_support::BEFORE_HELPER_MODE_PREFIX,
            auth_support::BEFORE_WRAPPER_SHA_PREFIX,
            auth_support::BEFORE_WRAPPER_MODE_PREFIX,
        )
    };
    Ok(AuthSupportEvidence {
        helper: support_file_from_markers(
            &plan.auth_helper,
            &unique_marker(text, helper_sha)?,
            &unique_marker(text, helper_mode)?,
            after.then_some(plan.release_authority.auth_helper.blob_oid.as_str()),
            after,
            plan,
        )?,
        wrapper: support_file_from_markers(
            &plan.auth_wrapper,
            &unique_marker(text, wrapper_sha)?,
            &unique_marker(text, wrapper_mode)?,
            after.then_some(plan.release_authority.auth_wrapper.blob_oid.as_str()),
            after,
            plan,
        )?,
    })
}

fn support_file_from_markers(
    path: &Path,
    sha256: &str,
    mode: &str,
    blob_oid: Option<&str>,
    verified: bool,
    plan: &HostUpdatePlan,
) -> Result<SupportFileEvidence, PlanExecutionError> {
    let (sha256, mode) = match (sha256, mode) {
        ("absent", "absent") => (None, None),
        ("absent", _) | (_, "absent") => {
            return Err(PlanExecutionError::Failed(
                "auth support presence markers disagreed".to_owned(),
            ));
        }
        (sha256, mode) => {
            if !valid_sha256(sha256) {
                return Err(PlanExecutionError::Failed(
                    "auth support SHA-256 marker was invalid".to_owned(),
                ));
            }
            let parsed = u32::from_str_radix(mode, 8).map_err(|_| {
                PlanExecutionError::Failed("auth support mode marker was invalid".to_owned())
            })?;
            (Some(sha256.to_owned()), Some(parsed))
        }
    };
    Ok(SupportFileEvidence {
        path: path.to_owned(),
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
    Ok(())
}

fn ssh_binary() -> PathBuf {
    [PathBuf::from("/usr/bin/ssh"), PathBuf::from("/bin/ssh")]
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("ssh"))
}

#[cfg(all(test, unix))]
mod tests;
