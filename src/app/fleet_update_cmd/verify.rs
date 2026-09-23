//! Independent post-rollout verification for one host.
//!
//! The update transaction already validates its own receipt. This is a second,
//! separate read after it finishes: a fresh process on the host asks the
//! installed binary its version, asks the running daemon who it is, and, when
//! the release ships the `ghapp` queue guards, installs this release's guards
//! and confirms they are current. A host is only counted as updated once this
//! read agrees with the target, so "the release reached the fleet" is a
//! measured fact rather than an inference from a successful exit code.

use std::process::Command;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use super::HostUpdatePlan;
use crate::executor::ssh::shlex_quote;
use crate::paths::{home_dir, unattended_tool_path};

/// Upper bound for one host's verification, including the guards install.
pub(super) const HOST_VERIFY_TIMEOUT: Duration = Duration::from_mins(2);

const CLI_PREFIX: &str = "SHIPYARD_VERIFY_CLI=";
const DAEMON_PREFIX: &str = "SHIPYARD_VERIFY_DAEMON=";
const PIDFILE_PREFIX: &str = "SHIPYARD_VERIFY_DAEMON_PID=";
const PID_ALIVE_PREFIX: &str = "SHIPYARD_VERIFY_DAEMON_PID_ALIVE=";
const GUARDS_BEFORE_PREFIX: &str = "SHIPYARD_VERIFY_GUARDS_BEFORE=";
const GUARDS_INSTALL_PREFIX: &str = "SHIPYARD_VERIFY_GUARDS_INSTALL=";
const GUARDS_INSTALL_EXIT_PREFIX: &str = "SHIPYARD_VERIFY_GUARDS_INSTALL_EXIT=";
const GUARDS_PREFIX: &str = "SHIPYARD_VERIFY_GUARDS=";
const GUARDS_UNSUPPORTED: &str = "unsupported";

/// Per-host verification receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct HostVerification {
    pub(super) host_class: String,
    pub(super) ssh: Option<String>,
    /// `verified` or `failed`.
    pub(super) verdict: String,
    pub(super) expected_version: String,
    pub(super) cli_version: Option<String>,
    pub(super) daemon_version: Option<String>,
    pub(super) daemon_pid: Option<u64>,
    pub(super) expected_daemon_pid: u32,
    /// `current`, `unsupported` (the release ships no guards), or a failure.
    pub(super) guards: String,
    pub(super) guards_installed: bool,
    /// Guards whose installed copy differed from this release's and was
    /// replaced (a previous release's copy, or a local edit), with the
    /// replaced content hash. Always reported, never replaced silently.
    pub(super) guards_replaced: Vec<String>,
    pub(super) failures: Vec<String>,
}

impl HostVerification {
    pub(super) fn verified(&self) -> bool {
        self.verdict == "verified"
    }
}

/// The script one fresh process runs on the host after the update.
pub(super) fn verify_script(plan: &HostUpdatePlan) -> String {
    let binary = shlex_quote(&plan.binary.display().to_string());
    let daemon = format!(
        "{binary} --mode {} --global-dir {} --state-dir {} --json daemon status",
        shlex_quote(plan.runtime_mode.as_str()),
        shlex_quote(&plan.global_dir.display().to_string()),
        shlex_quote(&plan.state_dir.display().to_string()),
    );
    let pid_file = shlex_quote(
        &plan
            .state_dir
            .join("daemon")
            .join("daemon.pid")
            .display()
            .to_string(),
    );
    format!(
        "set -u\n\
         printf '%s%s\\n' {cli} \"$({binary} --version 2>/dev/null | /usr/bin/head -n 1)\"\n\
         printf '%s%s\\n' {daemon_prefix} \"$({daemon} 2>/dev/null | /usr/bin/tr -d '\\n')\"\n\
         pid=\"$(/usr/bin/tr -dc '0-9' < {pid_file} 2>/dev/null)\"\n\
         printf '%s%s\\n' {pid_prefix} \"$pid\"\n\
         if [ -n \"$pid\" ] && /bin/kill -0 \"$pid\" 2>/dev/null; then printf '%s1\\n' {alive}; else printf '%s0\\n' {alive}; fi\n\
         if {binary} guards --help >/dev/null 2>&1; then\n\
         \x20 printf '%s%s\\n' {before} \"$({binary} --json guards status 2>/dev/null | /usr/bin/tr -d '\\n')\"\n\
         \x20 install_out=\"$({binary} --json guards install 2>/dev/null)\"; install_exit=$?\n\
         \x20 printf '%s%s\\n' {install} \"$(printf '%s' \"$install_out\" | /usr/bin/tr -d '\\n')\"\n\
         \x20 printf '%s%s\\n' {install_exit} \"$install_exit\"\n\
         \x20 printf '%s%s\\n' {guards} \"$({binary} --json guards status 2>/dev/null | /usr/bin/tr -d '\\n')\"\n\
         else\n\
         \x20 printf '%s%s\\n' {guards} {unsupported}\n\
         fi\n",
        cli = shlex_quote(CLI_PREFIX),
        pid_prefix = shlex_quote(PIDFILE_PREFIX),
        alive = shlex_quote(PID_ALIVE_PREFIX),
        daemon_prefix = shlex_quote(DAEMON_PREFIX),
        install = shlex_quote(GUARDS_INSTALL_PREFIX),
        install_exit = shlex_quote(GUARDS_INSTALL_EXIT_PREFIX),
        before = shlex_quote(GUARDS_BEFORE_PREFIX),
        guards = shlex_quote(GUARDS_PREFIX),
        unsupported = shlex_quote(GUARDS_UNSUPPORTED),
    )
}

/// Run [`verify_script`] on the plan's host and judge the result.
pub(super) fn verify_host(plan: &HostUpdatePlan, expected_daemon_pid: u32) -> HostVerification {
    let deadline = Instant::now() + HOST_VERIFY_TIMEOUT;
    let script = verify_script(plan);
    let mut command = if let Some(host) = &plan.ssh {
        let mut command = Command::new(super::evidence::ssh_binary_path());
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "StrictHostKeyChecking=yes",
        ]);
        command.arg(host).arg(&script);
        command
    } else {
        let mut command = Command::new("/bin/bash");
        command
            .args(["-c", &script])
            .env_clear()
            .env("HOME", home_dir())
            .env("PATH", unattended_tool_path());
        command
    };
    let label = format!("post-rollout verification for host class {}", plan.class);
    match crate::process::run_output_until(&mut command, deadline, &label) {
        Ok(output) if output.status.success() => judge(
            plan,
            expected_daemon_pid,
            &String::from_utf8_lossy(&output.stdout),
        ),
        Ok(output) => failed(
            plan,
            expected_daemon_pid,
            format!(
                "verification process exited {}",
                output.status.code().unwrap_or(-1)
            ),
        ),
        Err(error) => failed(plan, expected_daemon_pid, error.to_string()),
    }
}

fn failed(plan: &HostUpdatePlan, expected_daemon_pid: u32, reason: String) -> HostVerification {
    HostVerification {
        host_class: plan.class.clone(),
        ssh: plan.ssh.clone(),
        verdict: "failed".to_owned(),
        expected_version: plan.target.trim_start_matches('v').to_owned(),
        cli_version: None,
        daemon_version: None,
        daemon_pid: None,
        expected_daemon_pid,
        guards: "unverified".to_owned(),
        guards_installed: false,
        guards_replaced: Vec::new(),
        failures: vec![reason],
    }
}

fn marker<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let mut found = text.lines().filter_map(|line| line.strip_prefix(prefix));
    let value = found.next()?;
    // A duplicated marker means the output cannot be attributed to one read.
    found.next().is_none().then_some(value)
}

/// Judge one verification transcript. Pure, so every branch is testable.
pub(super) fn judge(
    plan: &HostUpdatePlan,
    expected_daemon_pid: u32,
    stdout: &str,
) -> HostVerification {
    let expected_version = plan.target.trim_start_matches('v').to_owned();
    let mut failures = Vec::new();

    let cli_version = marker(stdout, CLI_PREFIX)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    match cli_version.as_deref() {
        Some(value) if value == format!("shipyard {expected_version}") => {}
        Some(value) => failures.push(format!(
            "installed CLI reports {value:?}, expected \"shipyard {expected_version}\""
        )),
        None => failures.push("installed CLI version could not be read".to_owned()),
    }

    let status =
        marker(stdout, DAEMON_PREFIX).and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let running = status
        .as_ref()
        .and_then(|value| value.get("running"))
        .and_then(Value::as_bool);
    let daemon_version = status
        .as_ref()
        .and_then(|value| value.get("shipyard_version"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    // The status frame carries no pid; the daemon's own pid file names the
    // process, and liveness is checked on the host in the same read.
    let daemon_pid = marker(stdout, PIDFILE_PREFIX).and_then(|raw| raw.trim().parse::<u64>().ok());
    let pid_alive = marker(stdout, PID_ALIVE_PREFIX) == Some("1");
    match (status.as_ref(), running) {
        (None, _) => failures.push("daemon status could not be read".to_owned()),
        (Some(_), Some(true)) => {}
        (Some(_), _) => failures.push("daemon is not running after the rollout".to_owned()),
    }
    if status.is_some() && running == Some(true) {
        if daemon_version.as_deref() != Some(expected_version.as_str()) {
            failures.push(format!(
                "daemon answers as version {daemon_version:?}, expected {expected_version:?}"
            ));
        }
        if daemon_pid != Some(u64::from(expected_daemon_pid)) {
            failures.push(format!(
                "daemon pid file names {daemon_pid:?}, not the refreshed pid {expected_daemon_pid}"
            ));
        } else if !pid_alive {
            failures.push(format!(
                "refreshed daemon pid {expected_daemon_pid} is not alive"
            ));
        }
    }

    let mut guards_replaced = Vec::new();
    let (guards, guards_installed) = match marker(stdout, GUARDS_PREFIX) {
        Some(GUARDS_UNSUPPORTED) => (GUARDS_UNSUPPORTED.to_owned(), false),
        Some(raw) => {
            guards_replaced = replaced_guards(marker(stdout, GUARDS_BEFORE_PREFIX));
            let installed = match install_verdict(stdout) {
                Ok(()) => true,
                Err(reason) => {
                    failures.push(format!("ghapp guards install: {reason}"));
                    false
                }
            };
            match guards_verdict(raw) {
                Ok(()) => ("current".to_owned(), installed),
                Err(reason) => {
                    failures.push(format!("ghapp guards: {reason}"));
                    (reason, installed)
                }
            }
        }
        None => {
            failures.push("ghapp guards state could not be read".to_owned());
            ("unverified".to_owned(), false)
        }
    };

    HostVerification {
        host_class: plan.class.clone(),
        ssh: plan.ssh.clone(),
        verdict: if failures.is_empty() {
            "verified"
        } else {
            "failed"
        }
        .to_owned(),
        expected_version,
        cli_version,
        daemon_version,
        daemon_pid,
        expected_daemon_pid,
        guards,
        guards_installed,
        guards_replaced,
        failures,
    }
}

/// The install's own exit code and per-guard actions are authoritative: a
/// non-zero exit, an unreadable receipt, or any refused guard fails the host.
fn install_verdict(stdout: &str) -> Result<(), String> {
    let exit = marker(stdout, GUARDS_INSTALL_EXIT_PREFIX)
        .ok_or_else(|| "exit status was not reported".to_owned())?
        .trim()
        .to_owned();
    let receipt = marker(stdout, GUARDS_INSTALL_PREFIX)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let refused = receipt
        .as_ref()
        .and_then(|value| value.get("guards"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|row| {
                    row.get("action").and_then(Value::as_str) != Some("installed")
                        && row.get("action").and_then(Value::as_str) != Some("replaced")
                        && row.get("action").and_then(Value::as_str) != Some("unchanged")
                })
                .map(|row| {
                    format!(
                        "{} {}{}",
                        row.get("name").and_then(Value::as_str).unwrap_or("?"),
                        row.get("action").and_then(Value::as_str).unwrap_or("?"),
                        row.get("detail")
                            .and_then(Value::as_str)
                            .map_or_else(String::new, |detail| format!(" ({detail})"))
                    )
                })
                .collect::<Vec<_>>()
        });
    if exit != "0" {
        return Err(match refused.filter(|rows| !rows.is_empty()) {
            Some(rows) => format!("exited {exit}: {}", rows.join(", ")),
            None => format!("exited {exit}"),
        });
    }
    match refused {
        None => Err("its receipt was not readable".to_owned()),
        Some(rows) if !rows.is_empty() => Err(rows.join(", ")),
        Some(_) => Ok(()),
    }
}

/// Guards whose pre-install copy differed from the release's (status
/// `stale`), named with the hash that was replaced.
fn replaced_guards(before: Option<&str>) -> Vec<String> {
    before
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| value.get("guards").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter(|row| row.get("status").and_then(Value::as_str) == Some("stale"))
        .map(|row| {
            format!(
                "{} (replaced sha256 {})",
                row.get("name").and_then(Value::as_str).unwrap_or("?"),
                row.get("installed_sha256")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            )
        })
        .collect()
}

fn guards_verdict(raw: &str) -> Result<(), String> {
    let value =
        serde_json::from_str::<Value>(raw).map_err(|_| "guards status was not JSON".to_owned())?;
    let rows = value
        .get("guards")
        .and_then(Value::as_array)
        .filter(|rows| !rows.is_empty())
        .ok_or_else(|| "guards status listed no guards".to_owned())?;
    let stale = rows
        .iter()
        .filter(|row| row.get("status").and_then(Value::as_str) != Some("current"))
        .map(|row| {
            format!(
                "{}={}",
                row.get("name").and_then(Value::as_str).unwrap_or("?"),
                row.get("status").and_then(Value::as_str).unwrap_or("?")
            )
        })
        .collect::<Vec<_>>();
    if stale.is_empty() {
        Ok(())
    } else {
        Err(format!("not current: {}", stale.join(", ")))
    }
}
