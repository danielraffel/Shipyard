//! Bounded rollout execution and typed post-install evidence collection.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    HOST_UPDATE_TIMEOUT, HostUpdatePlan, REMOTE_AFTER_STATUS_PREFIX, REMOTE_BEFORE_STATUS_PREFIX,
    REMOTE_CLI_VERSION_PREFIX, REMOTE_REFRESH_PREFIX, REMOTE_SHA256_PREFIX,
};
use crate::paths::{home_dir, unattended_tool_path};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HostUpdateEvidence {
    pub(super) executable_sha256: String,
    pub(super) cli_version: String,
    pub(super) daemon_version: String,
    pub(super) daemon_pid: u32,
    pub(super) configured_repos_before: Option<Vec<String>>,
    pub(super) configured_repos_after: Vec<String>,
    pub(super) configured_repos_preserved: Option<bool>,
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
    let before_status = if plan.ssh.is_none() {
        Some(run_local_daemon_status(plan, deadline)?)
    } else {
        None
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
    update_stdout: &[u8],
    host_deadline: Instant,
) -> Result<HostUpdateEvidence, PlanExecutionError> {
    let cli_version = command_version(&plan.binary, plan, host_deadline)?;
    ensure_before_deadline(host_deadline, "installed executable hash")?;
    let executable_sha256 = sha256_file(&plan.binary).map_err(|error| {
        PlanExecutionError::Failed(format!(
            "failed to hash installed binary {}: {error}",
            plan.binary.display()
        ))
    })?;
    ensure_before_deadline(host_deadline, "installed executable hash")?;
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
        executable_sha256,
        cli_version,
        daemon_pid,
        before_status,
        &after_status,
    )
}

pub(super) fn parse_remote_evidence(
    plan: &HostUpdatePlan,
    stdout: &[u8],
) -> Result<HostUpdateEvidence, PlanExecutionError> {
    let text = std::str::from_utf8(stdout).map_err(|_| {
        PlanExecutionError::Failed("remote fleet evidence was not UTF-8".to_owned())
    })?;
    let executable_sha256 = unique_marker(text, REMOTE_SHA256_PREFIX)?;
    let cli_version = unique_marker(text, REMOTE_CLI_VERSION_PREFIX)?;
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
        executable_sha256,
        cli_version,
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
    String::from_utf8(output.stdout)
        .map(|version| version.trim().to_owned())
        .map_err(|_| PlanExecutionError::Failed("installed CLI version was not UTF-8".to_owned()))
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
    executable_sha256: String,
    cli_version: String,
    daemon_pid: u32,
    before_status: &Value,
    after_status: &Value,
) -> Result<HostUpdateEvidence, PlanExecutionError> {
    if !valid_sha256(&executable_sha256) {
        return Err(PlanExecutionError::Failed(
            "installed executable SHA-256 was not a lowercase 64-character digest".to_owned(),
        ));
    }
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
        executable_sha256,
        cli_version,
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

pub(super) fn validate_evidence(
    plan: &HostUpdatePlan,
    evidence: &HostUpdateEvidence,
) -> Result<(), String> {
    let version = plan.target.trim_start_matches('v');
    let expected_cli = format!("shipyard {version}");
    if evidence.cli_version != expected_cli {
        return Err(format!(
            "installed CLI version mismatch: expected {expected_cli:?}, observed {:?}",
            evidence.cli_version
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

    fn evidence(version: &str) -> HostUpdateEvidence {
        HostUpdateEvidence {
            executable_sha256: "a".repeat(64),
            cli_version: format!("shipyard {version}"),
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
            "{REMOTE_SHA256_PREFIX}{}\n{REMOTE_CLI_VERSION_PREFIX}shipyard 0.126.3\n{REMOTE_BEFORE_STATUS_PREFIX}{before}\n{REMOTE_REFRESH_PREFIX}{refresh}\n{REMOTE_AFTER_STATUS_PREFIX}{after}\n",
            "b".repeat(64)
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
        assert!(!valid_sha256(&"A".repeat(64)));
        assert!(!valid_sha256("short"));
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
        let observed = evidence_from_values(
            "c".repeat(64),
            "shipyard 0.126.3".to_owned(),
            9,
            &before,
            &after,
        )
        .expect("fresh evidence");
        assert_eq!(observed.configured_repos_before, None);
        assert_eq!(observed.configured_repos_preserved, None);
    }

    #[test]
    fn remote_evidence_rejects_duplicate_or_incomplete_markers() {
        let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.126.3").expect("plan");
        let duplicate = format!(
            "{REMOTE_SHA256_PREFIX}{}\n{REMOTE_SHA256_PREFIX}{}\n",
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
