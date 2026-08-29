//! Bounded rollout execution and typed post-install evidence collection.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[cfg(test)]
use super::release_source_identity;
use super::{
    COMPANION_BINARY_NAME, HOST_UPDATE_TIMEOUT, HostUpdatePlan,
    REMOTE_AFTER_COMPANION_SHA256_PREFIX, REMOTE_AFTER_COMPANION_VERSION_PREFIX,
    REMOTE_AFTER_PRIMARY_SHA256_PREFIX, REMOTE_AFTER_PRIMARY_VERSION_PREFIX,
    REMOTE_AFTER_STATUS_PREFIX, REMOTE_BEFORE_COMPANION_SHA256_PREFIX,
    REMOTE_BEFORE_COMPANION_VERSION_PREFIX, REMOTE_BEFORE_PRIMARY_SHA256_PREFIX,
    REMOTE_BEFORE_PRIMARY_VERSION_PREFIX, REMOTE_BEFORE_STATUS_PREFIX, REMOTE_REFRESH_PREFIX,
    tag_requires_companion,
};
use crate::paths::{home_dir, unattended_tool_path};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HostUpdateEvidence {
    pub(super) before_pair: BinaryPairEvidence,
    pub(super) after_pair: BinaryPairEvidence,
    pub(super) executable_sha256: String,
    pub(super) cli_version: String,
    pub(super) daemon_version: String,
    pub(super) daemon_pid: u32,
    pub(super) configured_repos_before: Option<Vec<String>>,
    pub(super) configured_repos_after: Vec<String>,
    pub(super) configured_repos_preserved: Option<bool>,
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
    VerifiedInstallerTarget,
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
    let (before_status, before_pair) = if plan.ssh.is_none() {
        let status = run_local_daemon_status(plan, deadline)?;
        let pair = collect_local_pair(plan, deadline, false)?;
        validate_binary_pair(plan, &pair, None).map_err(PlanExecutionError::Failed)?;
        (Some(status), Some(pair))
    } else {
        (None, None)
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
        let mut command = Command::new(&plan.binary);
        command
            .args(["--mode", plan.runtime_mode.as_str(), "--global-dir"])
            .arg(&plan.global_dir)
            .arg("--state-dir")
            .arg(&plan.state_dir)
            .args([
                "--json",
                "update",
                "--to",
                &plan.target,
                "--refresh-daemon",
                "--unattended-fleet",
            ])
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
    update_stdout: &[u8],
    host_deadline: Instant,
) -> Result<HostUpdateEvidence, PlanExecutionError> {
    let after_pair = collect_local_pair(plan, host_deadline, true)?;
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
        daemon_pid,
        before_status,
        &after_status,
    )
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
            SourceIdentityBasis::VerifiedInstallerTarget
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
    let evidence = evidence_from_values(
        before_pair,
        after_pair,
        daemon_pid,
        &before_status,
        &after_status,
    )?;
    if plan.ssh.is_none() {
        return Err(PlanExecutionError::Failed(
            "remote evidence was returned for a local plan".to_owned(),
        ));
    }
    Ok(evidence)
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
            SourceIdentityBasis::VerifiedInstallerTarget
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
                    SourceIdentityBasis::VerifiedInstallerTarget
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

fn evidence_from_values(
    before_pair: BinaryPairEvidence,
    after_pair: BinaryPairEvidence,
    daemon_pid: u32,
    before_status: &Value,
    after_status: &Value,
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
        executable_sha256: after_pair.primary.sha256.clone(),
        cli_version: format!("shipyard {}", after_pair.primary.semantic_version),
        before_pair,
        after_pair,
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
                    != SourceIdentityBasis::VerifiedInstallerTarget =>
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
    validate_binary_pair(plan, &evidence.before_pair, None)?;
    validate_binary_pair(plan, &evidence.after_pair, Some(&plan.target))?;
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

fn ssh_binary() -> PathBuf {
    [PathBuf::from("/usr/bin/ssh"), PathBuf::from("/bin/ssh")]
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("ssh"))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::process::Stdio;

    use super::*;
    use crate::capacity::HostClassConfig;

    fn host(ssh: Option<&str>) -> HostClassConfig {
        HostClassConfig {
            class: "m5".to_owned(),
            ssh: ssh.map(str::to_owned),
            cap: 2,
            tart_bin: "/opt/homebrew/bin/tart".to_owned(),
            tartci_bin: "/Users/ci/.local/bin/tartci".to_owned(),
            shipyard_bin: Some("/Users/ci/.local/bin/shipyard".to_owned()),
            shipyard_mode: Some("shipyard".to_owned()),
            shipyard_global_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
            shipyard_state_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
            github_cli: Some("/Users/ci/.local/bin/ghapp".to_owned()),
            tart_home: Some("/Users/ci/VMs".to_owned()),
            labels: Vec::new(),
        }
    }

    fn pair(version: &str, verified: bool) -> BinaryPairEvidence {
        let source_identity = verified.then(|| release_source_identity(&format!("v{version}")));
        let source_identity_basis = if verified {
            SourceIdentityBasis::VerifiedInstallerTarget
        } else {
            SourceIdentityBasis::UnverifiedPreinstall
        };
        let primary = BinaryEvidence {
            path: PathBuf::from("/Users/ci/.local/bin/shipyard"),
            semantic_version: version.to_owned(),
            sha256: "a".repeat(64),
            source_identity: source_identity.clone(),
            source_identity_basis,
        };
        let companion = tag_requires_companion(&format!("v{version}")).then(|| BinaryEvidence {
            path: PathBuf::from("/Users/ci/.local/bin/shipyard-workstream-provider"),
            semantic_version: version.to_owned(),
            sha256: "b".repeat(64),
            source_identity,
            source_identity_basis,
        });
        BinaryPairEvidence { primary, companion }
    }

    fn evidence(version: &str) -> HostUpdateEvidence {
        HostUpdateEvidence {
            executable_sha256: "a".repeat(64),
            cli_version: format!("shipyard {version}"),
            before_pair: pair(version, false),
            after_pair: pair(version, true),
            daemon_version: version.to_owned(),
            daemon_pid: 42,
            configured_repos_before: Some(vec!["owner/repo".to_owned()]),
            configured_repos_after: vec!["owner/repo".to_owned()],
            configured_repos_preserved: Some(true),
        }
    }

    #[test]
    fn remote_evidence_is_typed_and_proves_repo_preservation() {
        let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.126.3").expect("plan");
        let before = serde_json::json!({
            "command": "daemon:status",
            "running": true,
            "configured_repos": ["owner/b", "owner/a"],
            "shipyard_version": "0.126.2"
        });
        let refresh = serde_json::json!({
            "command": "daemon:refresh",
            "new_pid": 4242,
            "repos": ["owner/a", "owner/b"]
        });
        let after = serde_json::json!({
            "command": "daemon:status",
            "running": true,
            "configured_repos": ["owner/a", "owner/b"],
            "shipyard_version": "0.126.3"
        });
        let stdout = format!(
            "{REMOTE_BEFORE_PRIMARY_SHA256_PREFIX}{}\n{REMOTE_BEFORE_PRIMARY_VERSION_PREFIX}shipyard 0.126.2\n{REMOTE_BEFORE_COMPANION_SHA256_PREFIX}absent\n{REMOTE_BEFORE_COMPANION_VERSION_PREFIX}absent\n{REMOTE_AFTER_PRIMARY_SHA256_PREFIX}{}\n{REMOTE_AFTER_PRIMARY_VERSION_PREFIX}shipyard 0.126.3\n{REMOTE_AFTER_COMPANION_SHA256_PREFIX}{}\n{REMOTE_AFTER_COMPANION_VERSION_PREFIX}shipyard-workstream-provider 0.126.3\n{REMOTE_BEFORE_STATUS_PREFIX}{before}\n{REMOTE_REFRESH_PREFIX}{refresh}\n{REMOTE_AFTER_STATUS_PREFIX}{after}\n",
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
        );
        let evidence = parse_remote_evidence(&plan, stdout.as_bytes()).expect("evidence");
        assert_eq!(evidence.daemon_pid, 4242);
        assert_eq!(evidence.configured_repos_preserved, Some(true));
        validate_evidence(&plan, &evidence).expect("valid evidence");
    }

    #[test]
    fn evidence_rejects_version_repo_and_digest_drift() {
        let plan = super::super::host_update_plan(&host(None), "v0.126.3").expect("plan");
        let mut observed = evidence("0.126.3");
        observed.daemon_version = "0.126.2".to_owned();
        assert!(validate_evidence(&plan, &observed).is_err());
        observed.daemon_version = "0.126.3".to_owned();
        observed.configured_repos_preserved = Some(false);
        assert!(validate_evidence(&plan, &observed).is_err());
        observed.configured_repos_preserved = Some(true);
        observed.executable_sha256 = "d".repeat(64);
        assert!(validate_evidence(&plan, &observed).is_err());
        assert!(!valid_sha256(&"A".repeat(64)));
        assert!(!valid_sha256("short"));
    }

    #[test]
    fn evidence_never_accepts_mixed_pair_or_legacy_companion() {
        let paired_plan =
            super::super::host_update_plan(&host(None), "v0.126.3").expect("paired plan");
        let mut mixed = evidence("0.126.3");
        mixed
            .after_pair
            .companion
            .as_mut()
            .expect("companion")
            .semantic_version = "0.126.4".to_owned();
        assert!(validate_evidence(&paired_plan, &mixed).is_err());

        let mut wrong_source = evidence("0.126.3");
        wrong_source.after_pair.primary.source_identity = Some(release_source_identity("v0.126.4"));
        assert!(validate_evidence(&paired_plan, &wrong_source).is_err());

        let legacy_plan =
            super::super::host_update_plan(&host(None), "v0.126.2").expect("legacy plan");
        let mut legacy_mixed = evidence("0.126.2");
        legacy_mixed.after_pair.companion = Some(BinaryEvidence {
            path: legacy_plan.companion_binary.clone(),
            semantic_version: "0.126.2".to_owned(),
            sha256: "d".repeat(64),
            source_identity: Some(release_source_identity("v0.126.2")),
            source_identity_basis: SourceIdentityBasis::VerifiedInstallerTarget,
        });
        assert!(validate_evidence(&legacy_plan, &legacy_mixed).is_err());
    }

    #[test]
    fn evidence_never_infers_preinstall_provenance_or_omits_postinstall_binding() {
        let plan = super::super::host_update_plan(&host(None), "v0.126.3").expect("plan");

        let mut fabricated_before = evidence("0.126.3");
        fabricated_before.before_pair.primary.source_identity =
            Some(release_source_identity("v0.126.3"));
        fabricated_before.before_pair.primary.source_identity_basis =
            SourceIdentityBasis::VerifiedInstallerTarget;
        fabricated_before
            .before_pair
            .companion
            .as_mut()
            .expect("companion")
            .source_identity = Some(release_source_identity("v0.126.3"));
        fabricated_before
            .before_pair
            .companion
            .as_mut()
            .expect("companion")
            .source_identity_basis = SourceIdentityBasis::VerifiedInstallerTarget;
        assert!(validate_evidence(&plan, &fabricated_before).is_err());

        let mut unbound_after = evidence("0.126.3");
        unbound_after.after_pair.primary.source_identity = None;
        unbound_after.after_pair.primary.source_identity_basis =
            SourceIdentityBasis::UnverifiedPreinstall;
        unbound_after
            .after_pair
            .companion
            .as_mut()
            .expect("companion")
            .source_identity = None;
        unbound_after
            .after_pair
            .companion
            .as_mut()
            .expect("companion")
            .source_identity_basis = SourceIdentityBasis::UnverifiedPreinstall;
        assert!(validate_evidence(&plan, &unbound_after).is_err());
    }

    #[test]
    fn fresh_daemon_reports_preservation_as_not_applicable() {
        let before = serde_json::json!({"command": "daemon:status", "running": false});
        let after = serde_json::json!({
            "command": "daemon:status",
            "running": true,
            "configured_repos": [],
            "shipyard_version": "0.126.3"
        });
        let pair = evidence("0.126.3").after_pair;
        let observed =
            evidence_from_values(pair.clone(), pair, 9, &before, &after).expect("fresh evidence");
        assert_eq!(observed.configured_repos_before, None);
        assert_eq!(observed.configured_repos_preserved, None);
    }

    #[test]
    fn remote_evidence_rejects_duplicate_or_incomplete_markers() {
        let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.126.3").expect("plan");
        let duplicate = format!(
            "{REMOTE_BEFORE_PRIMARY_SHA256_PREFIX}{}\n{REMOTE_BEFORE_PRIMARY_SHA256_PREFIX}{}\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert!(parse_remote_evidence(&plan, duplicate.as_bytes()).is_err());
        assert!(parse_remote_evidence(&plan, b"").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn local_evidence_probes_share_the_host_attempt_deadline() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let binary = temp.path().join("shipyard");
        std::fs::write(
            &binary,
            "#!/bin/sh\ncase \"$*\" in *\"daemon status\"*) printf '%s\\n' '{\"command\":\"daemon:status\",\"running\":false}' ;; *\"--version\"*) sleep 60 ;; *) printf '%s\\n' '{\"event\":\"daemon_refreshed\",\"daemon_pid\":42}' ;; esac\n",
        )
        .expect("fixture");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        let mut class = host(None);
        class.shipyard_bin = Some(binary.display().to_string());
        let plan = super::super::host_update_plan(&class, "v0.100.0").expect("plan");
        let started = Instant::now();
        assert!(matches!(
            execute_plan_with_timeout(&plan, Duration::from_millis(100)),
            Err(PlanExecutionError::TimedOut(_))
        ));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "evidence probe received a fresh timeout after the host deadline"
        );
    }

    #[cfg(unix)]
    #[test]
    fn remote_supervisor_kills_term_ignoring_descendants_after_leader_exits() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pid_file = temp.path().join("descendant.pid");
        let worker = format!(
            "(trap '' TERM; echo $$ > {}; while :; do sleep 1; done) & wait $!",
            crate::executor::ssh::shlex_quote(&pid_file.display().to_string())
        );
        let status = Command::new("/usr/bin/perl")
            .args([
                "-e",
                super::super::REMOTE_SUPERVISOR,
                "1",
                "/bin/bash",
                "-c",
                &worker,
            ])
            .status()
            .expect("remote supervisor fixture");
        assert_eq!(status.code(), Some(124));

        let pid = std::fs::read_to_string(pid_file)
            .expect("descendant pid")
            .trim()
            .to_owned();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && Command::new("/bin/kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !Command::new("/bin/kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success()),
            "TERM-ignoring descendant survived the remote timeout boundary"
        );
    }
}
