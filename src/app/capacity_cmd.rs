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
use std::process::ExitCode;

use serde_json::Value;

use super::CliFailure;
use crate::capacity::{
    HostCapacity, any_unreadable, gather_configured_host_capacities, total_free,
};
use crate::config::LoadedConfig;
use crate::output::write_json_envelope;

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
    let hosts =
        gather_configured_host_capacities(&config.data).map_err(|e| CliFailure::new(2, e))?;

    if hosts.is_empty() {
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
