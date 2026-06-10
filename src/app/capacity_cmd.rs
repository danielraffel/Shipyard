//! CLI handler for `shipyard runner capacity` (#316 Part B).
//!
//! Reads each configured `[host_class.<name>]`'s running macOS VMs (locally for
//! the controller's own box, over SSH for the others), computes free slots
//! `Σ max(0, cap − running)`, and reports per-host + total. `tart list` does
//! not reliably include OS, so every running VM is enriched with
//! `tart get <name> --format json`; only macOS/darwin VMs consume this quota.
//! Fail-closed: an unreadable host contributes 0 free and exits non-zero so a
//! cron/script notices — silence must not read as success.
//!
//! All capacity math + parsing is the pure code in [`crate::capacity`]; this
//! module is the only place that shells out to `tart` and `ssh`.

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, ExitCode};

use serde_json::Value;

use super::CliFailure;
use crate::capacity::{
    HostCapacity, HostClassConfig, any_unreadable, is_macos_os, parse_host_classes,
    parse_tart_get_os, parse_tart_running_names, total_free,
};
use crate::config::LoadedConfig;
use crate::executor::ssh::shlex_quote;
use crate::output::write_json_envelope;

/// SSH options for a non-interactive, fail-fast probe: no prompts, short
/// connect timeout, accept new host keys so a fresh host doesn't hang.
fn ssh_probe_options() -> Vec<String> {
    [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

fn remote_tart_command(class: &HostClassConfig, args: &[&str]) -> String {
    let mut parts = Vec::new();
    if let Some(tart_home) = &class.tart_home {
        parts.push("env".to_owned());
        parts.push(format!("TART_HOME={}", shlex_quote(tart_home)));
    }
    parts.push(shlex_quote(&class.tart_bin));
    parts.extend(args.iter().map(|arg| shlex_quote(arg)));
    parts.join(" ")
}

/// Execute `tart` for one host class, locally or over SSH.
fn run_tart(class: &HostClassConfig, args: &[&str], label: &str) -> Result<String, String> {
    let output = if let Some(host) = &class.ssh {
        Command::new("ssh")
            .args(ssh_probe_options())
            .arg(host)
            .arg(remote_tart_command(class, args))
            .output()
    } else {
        let mut command = Command::new(&class.tart_bin);
        if let Some(tart_home) = &class.tart_home {
            command.env("TART_HOME", tart_home);
        }
        command.args(args).output()
    };

    let output = output.map_err(|error| {
        if class.ssh.is_some() {
            format!("ssh spawn failed: {error}")
        } else {
            format!("`{}` spawn failed: {error}", class.tart_bin)
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            format!("exit {}", output.status.code().unwrap_or(-1))
        } else {
            detail.lines().next().unwrap_or(detail).to_owned()
        };
        return Err(format!("{label} failed: {detail}"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Read the running macOS-VM count for one host class. Returns `Ok(count)` or
/// an `Err(reason)` that the caller records as an unreadable host
/// (fail-closed).
fn read_running(class: &HostClassConfig) -> Result<u32, String> {
    let list_stdout = run_tart(class, &["list", "--format", "json"], "tart list")?;
    let running_names = parse_tart_running_names(&list_stdout)?;
    let mut running_macos: u32 = 0;
    for name in running_names {
        let get_stdout = run_tart(class, &["get", &name, "--format", "json"], "tart get")?;
        let os = parse_tart_get_os(&get_stdout)?;
        if is_macos_os(&os) {
            running_macos = running_macos
                .checked_add(1)
                .ok_or_else(|| "implausibly many running macOS VMs".to_owned())?;
        }
    }
    Ok(running_macos)
}

/// Gather per-host capacity for every configured `[host_class.*]` (probing each
/// host). Shared by `runner capacity` and the reroute watcher (#316 Part C).
pub(super) fn gather(config: &LoadedConfig) -> Result<Vec<HostCapacity>, CliFailure> {
    let classes = parse_host_classes(&config.data).map_err(|e| CliFailure::new(2, e))?;
    Ok(classes.iter().map(probe).collect())
}

/// Probe one host class and fold the result into a [`HostCapacity`].
fn probe(class: &HostClassConfig) -> HostCapacity {
    let (running, source) = match read_running(class) {
        Ok(count) => (
            Some(count),
            if class.ssh.is_some() { "ssh" } else { "local" }.to_owned(),
        ),
        Err(reason) => (None, reason),
    };
    HostCapacity {
        class: class.class.clone(),
        ssh: class.ssh.clone(),
        cap: class.cap,
        running,
        source,
    }
}

fn host_to_json(host: &HostCapacity) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("class".to_owned(), Value::from(host.class.clone()));
    m.insert(
        "ssh".to_owned(),
        host.ssh.clone().map_or(Value::Null, Value::from),
    );
    m.insert("cap".to_owned(), Value::from(host.cap));
    m.insert(
        "running".to_owned(),
        host.running.map_or(Value::Null, Value::from),
    );
    m.insert("free".to_owned(), Value::from(host.free()));
    m.insert("readable".to_owned(), Value::from(host.readable()));
    m.insert("source".to_owned(), Value::from(host.source.clone()));
    Value::Object(m)
}

/// `shipyard runner capacity` — report VM-slot-aware free macOS capacity across
/// configured host classes. Exit 0 when every host was read; exit 1 when any
/// host was unreadable (its free slots are counted as 0).
pub(super) fn capacity_command<W: Write>(
    config: &LoadedConfig,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let classes = parse_host_classes(&config.data).map_err(|e| CliFailure::new(2, e))?;

    if classes.is_empty() {
        let msg = "No [host_class.<name>] entries configured. Add e.g.\n\n  \
                   [host_class.studio]\n  # ssh omitted for the controller's own box\n  \
                   cap = 2\n  tart_bin = \"/opt/homebrew/bin/tart\"\n  tart_home = \"/Users/<you>/VMs\"\n  \
                   labels = [\"self-hosted\", \"macos\", \"arm64\", \"shipyard-build-studio\"]\n";
        if json {
            let mut data = BTreeMap::new();
            data.insert("hosts".to_owned(), Value::Array(Vec::new()));
            data.insert("free".to_owned(), Value::from(0));
            data.insert("any_unreadable".to_owned(), Value::from(false));
            data.insert("configured".to_owned(), Value::from(false));
            write_json_envelope(stdout, "runner.capacity", data)
                .map_err(|e| CliFailure::new(1, format!("failed to write JSON: {e}")))?;
            return Ok(ExitCode::SUCCESS);
        }
        writeln!(stdout, "{msg}").ok();
        return Ok(ExitCode::SUCCESS);
    }

    let hosts: Vec<HostCapacity> = classes.iter().map(probe).collect();
    let free = total_free(&hosts);
    let unreadable = any_unreadable(&hosts);
    let exit = if unreadable {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    };

    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "hosts".to_owned(),
            Value::from(hosts.iter().map(host_to_json).collect::<Vec<_>>()),
        );
        data.insert("free".to_owned(), Value::from(free));
        data.insert("any_unreadable".to_owned(), Value::from(unreadable));
        data.insert("configured".to_owned(), Value::from(true));
        write_json_envelope(stdout, "runner.capacity", data)
            .map_err(|e| CliFailure::new(1, format!("failed to write JSON: {e}")))?;
        return Ok(exit);
    }

    // Human output doubles as the per-host decision log.
    writeln!(
        stdout,
        "{:<10}  {:<28}  {:>3}  {:>7}  {:>4}  SOURCE",
        "CLASS", "SSH", "CAP", "RUNNING", "FREE"
    )
    .ok();
    for host in &hosts {
        let ssh = host.ssh.clone().unwrap_or_else(|| "(local)".to_owned());
        let running = host
            .running
            .map_or_else(|| "?".to_owned(), |r| r.to_string());
        writeln!(
            stdout,
            "{:<10}  {:<28}  {:>3}  {:>7}  {:>4}  {}",
            host.class,
            ssh,
            host.cap,
            running,
            host.free(),
            host.source,
        )
        .ok();
    }
    writeln!(stdout, "\nfree macOS slots: {free}").ok();
    if unreadable {
        writeln!(
            stdout,
            "⚠︎ one or more hosts unreadable — counted as 0 free (fail-closed). \
             Free slots above are a lower bound."
        )
        .ok();
    }
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_probe_options_are_noninteractive() {
        let opts = ssh_probe_options();
        assert!(opts.iter().any(|o| o == "BatchMode=yes"));
        assert!(opts.iter().any(|o| o.starts_with("ConnectTimeout")));
    }

    #[test]
    fn remote_tart_command_sets_tart_home_and_quotes_args() {
        let class = HostClassConfig {
            class: "m5".to_owned(),
            ssh: Some("m5-ci".to_owned()),
            cap: 2,
            tart_bin: "/opt/homebrew/bin/tart".to_owned(),
            tart_home: Some("/Users/ci user/VMs".to_owned()),
            labels: Vec::new(),
        };
        assert_eq!(
            remote_tart_command(&class, &["get", "vm one", "--format", "json"]),
            "env TART_HOME='/Users/ci user/VMs' /opt/homebrew/bin/tart get 'vm one' --format json"
        );
    }

    #[test]
    fn host_to_json_marks_unreadable_host() {
        let host = HostCapacity {
            class: "m1".to_owned(),
            ssh: Some("macpro".to_owned()),
            cap: 2,
            running: None,
            source: "tart list failed: timed out".to_owned(),
        };
        let value = host_to_json(&host);
        assert_eq!(value["readable"], Value::from(false));
        assert_eq!(value["free"], Value::from(0));
        assert_eq!(value["running"], Value::Null);
    }
}
