//! VM-slot-aware macOS capacity accounting across host-class members (#316
//! Part B).
//!
//! macOS limits **2 running VMs per host** (XNU kernel quota
//! `hv_apple_isa_vm_quota`; see Pulp's `planning/2026-06-01-macos-ci-isolation-plan.md`
//! Appendix D) — not Tart, not a license. A multi-Mac fleet's free macOS
//! capacity is therefore:
//!
//! ```text
//! free = Σ_hosts max(0, cap_host − running_macos_vms_host)
//! ```
//!
//! `cap_host` defaults to [`DEFAULT_CAP`]; only the dedicated always-on Studio
//! may raise it (dev-kernel boot-arg). `running_macos_vms_host` is read live
//! from `tart list` plus `tart get <vm>` OS enrichment on each host, using the
//! configured Tart store when `tart_home` is set — the Studio also hosts
//! long-lived runner agents and ephemeral builders that consume its slots, so
//! the live count is the truth, not a static assumption.
//!
//! **Fail-closed:** a host whose VM state can't be read (SSH/`tart` error,
//! unparseable output) contributes `free = 0` and is flagged unreadable — it is
//! never counted as spare capacity. Silence must not read as success.
//!
//! This module is the pure logic: config parsing, `tart list` running-name
//! parsing, `tart get` OS parsing, and the free-slot math. SSH'ing each host and
//! shelling `tart` is the impure edge in the CLI handler.

use std::process::Command;

use serde::Deserialize;
use toml::{Table, Value as TomlValue};

use crate::executor::ssh::shlex_quote;

/// Default macOS VM slots per host: the XNU kernel quota (Appendix D).
pub const DEFAULT_CAP: u32 = 2;

/// Parsed `[host_class.<name>]` config.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostClassConfig {
    /// Class name from `[host_class.<name>]`, e.g. `studio`, `m1`, `m5`.
    pub class: String,
    /// SSH host (`user@host` or `host`). `None` means the controller's own box,
    /// read locally without SSH.
    pub ssh: Option<String>,
    /// macOS VM slot cap. Defaults to [`DEFAULT_CAP`].
    pub cap: u32,
    /// The `tart` binary to invoke. Defaults to `tart`; override when it is not
    /// on a non-interactive SSH `PATH`.
    pub tart_bin: String,
    /// The `tartci` binary/wrapper to invoke for host-local doctor/fleet
    /// probes. Defaults to `tartci`; override for non-interactive SSH.
    pub tartci_bin: String,
    /// Optional Tart store to expose as `TART_HOME` while reading this host.
    /// Use an absolute path; shell expansion is intentionally not performed.
    pub tart_home: Option<String>,
    /// Routing/pin labels this host class's runners carry (informational; the
    /// reroute watcher uses `<repo>-build-<class>` to target this host).
    pub labels: Vec<String>,
}

/// Parse `[host_class]` from merged config. Returns classes sorted by name for
/// stable output. An empty/absent section yields an empty vec.
///
/// # Errors
/// Returns a human-readable message when a class entry is malformed (not a
/// table, non-string `ssh`/`tart_bin`/`tartci_bin`/`tart_home`,
/// non-integer/negative `cap`, or a `labels` that is not an array of strings).
pub fn parse_host_classes(data: &Table) -> Result<Vec<HostClassConfig>, String> {
    let Some(classes) = data.get("host_class").and_then(TomlValue::as_table) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(classes.len());
    for (class, value) in classes {
        let table = value
            .as_table()
            .ok_or_else(|| format!("host_class.{class} must be a table"))?;
        let ssh = match table.get("ssh") {
            Some(TomlValue::String(s)) if !s.trim().is_empty() => Some(s.trim().to_owned()),
            None | Some(TomlValue::String(_)) => None,
            Some(_) => return Err(format!("host_class.{class}.ssh must be a string")),
        };
        let cap = match table.get("cap") {
            None => DEFAULT_CAP,
            Some(TomlValue::Integer(n)) if *n >= 0 => {
                u32::try_from(*n).map_err(|_| format!("host_class.{class}.cap is too large"))?
            }
            Some(_) => {
                return Err(format!(
                    "host_class.{class}.cap must be a non-negative integer"
                ));
            }
        };
        let tart_bin = match table.get("tart_bin") {
            None => "tart".to_owned(),
            Some(TomlValue::String(s)) if !s.trim().is_empty() => s.trim().to_owned(),
            Some(_) => return Err(format!("host_class.{class}.tart_bin must be a string")),
        };
        let tartci_bin = match table.get("tartci_bin") {
            None => "tartci".to_owned(),
            Some(TomlValue::String(s)) if !s.trim().is_empty() => s.trim().to_owned(),
            Some(_) => return Err(format!("host_class.{class}.tartci_bin must be a string")),
        };
        let tart_home = match table.get("tart_home") {
            Some(TomlValue::String(s)) if !s.trim().is_empty() => Some(s.trim().to_owned()),
            None | Some(TomlValue::String(_)) => None,
            Some(_) => return Err(format!("host_class.{class}.tart_home must be a string")),
        };
        let labels = match table.get("labels") {
            None => Vec::new(),
            Some(TomlValue::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("host_class.{class}.labels must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(format!("host_class.{class}.labels must be an array")),
        };
        out.push(HostClassConfig {
            class: class.clone(),
            ssh,
            cap,
            tart_bin,
            tartci_bin,
            tart_home,
            labels,
        });
    }
    out.sort_by(|a, b| a.class.cmp(&b.class));
    Ok(out)
}

/// One VM entry from `tart list --format json`. Tart has used both a
/// `"State": "running"` string and a `"Running": true` boolean across versions,
/// so we accept either.
#[derive(Debug, Clone, Deserialize)]
struct TartVm {
    #[serde(rename = "Name", alias = "name")]
    name: Option<String>,
    #[serde(rename = "State", alias = "state")]
    state: Option<String>,
    #[serde(rename = "Running", alias = "running")]
    running: Option<bool>,
}

impl TartVm {
    fn is_running(&self) -> bool {
        if self.running == Some(true) {
            return true;
        }
        self.state
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("running"))
    }
}

/// Parse running VM names from `tart list --format json` output.
///
/// # Errors
/// Returns a message when the output is not the expected JSON array — the
/// caller treats that as an unreadable host (fail-closed), never as zero
/// running VMs (which would falsely advertise free capacity). A running VM with
/// no name is also an error because the CLI edge must call `tart get <name>` to
/// discover whether it consumes a macOS slot.
pub fn parse_tart_running_names(json: &str) -> Result<Vec<String>, String> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Err("empty `tart list` output".to_owned());
    }
    let vms: Vec<TartVm> = serde_json::from_str(trimmed)
        .map_err(|error| format!("could not parse `tart list --format json`: {error}"))?;
    let mut names = Vec::new();
    for vm in vms.iter().filter(|vm| vm.is_running()) {
        let name = vm
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| "running VM missing Name in `tart list` output".to_owned())?;
        names.push(name.to_owned());
    }
    Ok(names)
}

/// Parse the OS field from `tart get <vm> --format json` output.
///
/// Tart currently reports macOS images as `OS = "darwin"`. Missing or
/// malformed OS is an error: without it, capacity cannot know whether a running
/// VM consumes the macOS-only AVF quota.
pub fn parse_tart_get_os(json: &str) -> Result<String, String> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Err("empty `tart get` output".to_owned());
    }
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|error| format!("could not parse `tart get --format json`: {error}"))?;
    value
        .get("OS")
        .or_else(|| value.get("os"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|os| !os.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "`tart get --format json` missing OS".to_owned())
}

/// Whether a Tart OS identifier consumes the macOS-only AVF slot quota.
#[must_use]
pub fn is_macos_os(os: &str) -> bool {
    matches!(
        os.trim().to_ascii_lowercase().as_str(),
        "darwin" | "macos" | "macosx" | "mac os"
    )
}

/// Per-host capacity after attempting to read its running VMs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostCapacity {
    /// Host class name.
    pub class: String,
    /// SSH host, or `None` for the local controller box.
    pub ssh: Option<String>,
    /// Configured slot cap.
    pub cap: u32,
    /// Running macOS VMs, or `None` when the host couldn't be read.
    pub running: Option<u32>,
    /// `"local"`, `"ssh"`, or an error reason when unreadable.
    pub source: String,
}

impl HostCapacity {
    /// Whether the host's VM state was read successfully.
    #[must_use]
    pub fn readable(&self) -> bool {
        self.running.is_some()
    }

    /// Free slots on this host. **Fail-closed:** an unreadable host has 0 free.
    #[must_use]
    pub fn free(&self) -> u32 {
        match self.running {
            Some(running) => self.cap.saturating_sub(running),
            None => 0,
        }
    }
}

/// Total free macOS slots across hosts: `Σ max(0, cap − running)`, with
/// unreadable hosts contributing 0 (fail-closed).
#[must_use]
pub fn total_free(hosts: &[HostCapacity]) -> u32 {
    hosts.iter().map(HostCapacity::free).sum()
}

/// Whether any host could not be read — the caller should treat the total as a
/// lower bound and refuse to act if it needs a trustworthy count.
#[must_use]
pub fn any_unreadable(hosts: &[HostCapacity]) -> bool {
    hosts.iter().any(|host| !host.readable())
}

/// Gather per-host capacity for every configured `[host_class.*]`, probing each
/// host locally or over SSH. Shared by runner commands and queue admission so
/// reporting and scheduling use the same live snapshot.
///
/// # Errors
/// Returns a config parse error before probing when the host-class section is
/// malformed. Individual host probe failures are folded into unreadable
/// [`HostCapacity`] rows instead of failing the whole snapshot.
pub fn gather_configured_host_capacities(data: &Table) -> Result<Vec<HostCapacity>, String> {
    let classes = parse_host_classes(data)?;
    Ok(classes.iter().map(probe_host_capacity).collect())
}

/// Probe one host class and fold the result into a [`HostCapacity`].
#[must_use]
pub fn probe_host_capacity(class: &HostClassConfig) -> HostCapacity {
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

/// SSH options for non-interactive, fail-fast capacity probes.
#[must_use]
pub fn ssh_probe_options() -> Vec<String> {
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

/// Render the remote `tart` command used for one host-class probe.
#[must_use]
pub fn remote_tart_command(class: &HostClassConfig, args: &[&str]) -> String {
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

/// Read the running macOS-VM count for one host class.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn table(input: &str) -> Table {
        input.parse().expect("toml")
    }

    #[test]
    fn parse_host_classes_defaults_cap_and_reads_fields() {
        let cfg = table(
            r#"
            [host_class.studio]
            ssh = "studio-ci.local"
            cap = 4
            tart_home = "/Users/ci/VMs"
            labels = ["self-hosted", "shipyard-build-studio"]

            [host_class.m1]
            ssh = "m1-ci.local"
            "#,
        );
        let classes = parse_host_classes(&cfg).expect("parse");
        assert_eq!(classes.len(), 2);
        // sorted by name: m1 before studio
        assert_eq!(classes[0].class, "m1");
        assert_eq!(classes[0].cap, DEFAULT_CAP);
        assert_eq!(classes[0].tart_bin, "tart");
        assert_eq!(classes[0].tartci_bin, "tartci");
        assert_eq!(classes[1].class, "studio");
        assert_eq!(classes[1].cap, 4);
        assert_eq!(classes[1].ssh.as_deref(), Some("studio-ci.local"));
        assert_eq!(classes[1].tart_home.as_deref(), Some("/Users/ci/VMs"));
        assert_eq!(
            classes[1].labels,
            vec!["self-hosted", "shipyard-build-studio"]
        );
    }

    #[test]
    fn parse_host_classes_absent_section_is_empty() {
        assert!(
            parse_host_classes(&table("[project]\nname=\"x\"\n"))
                .expect("parse")
                .is_empty()
        );
    }

    #[test]
    fn parse_host_classes_local_box_has_no_ssh() {
        let cfg = table("[host_class.studio]\ncap = 2\n");
        let classes = parse_host_classes(&cfg).expect("parse");
        assert_eq!(classes[0].ssh, None);
    }

    #[test]
    fn parse_host_classes_rejects_bad_cap() {
        let cfg = table("[host_class.studio]\ncap = -1\n");
        assert!(parse_host_classes(&cfg).is_err());
        let cfg = table("[host_class.studio]\ncap = \"two\"\n");
        assert!(parse_host_classes(&cfg).is_err());
    }

    #[test]
    fn parse_host_classes_rejects_bad_tart_home() {
        let cfg = table("[host_class.studio]\ntart_home = 123\n");
        assert!(parse_host_classes(&cfg).is_err());
    }

    #[test]
    fn parse_host_classes_rejects_bad_tartci_bin() {
        let cfg = table("[host_class.studio]\ntartci_bin = []\n");
        assert!(parse_host_classes(&cfg).is_err());
    }

    #[test]
    fn parse_tart_running_names_accepts_running_state_string() {
        let json = r#"[
            {"Source":"local","Name":"shipyard-build-runner-abc","State":"running"},
            {"Source":"local","Name":"golden","State":"stopped"},
            {"Source":"local","Name":"other","State":"Running"}
        ]"#;
        assert_eq!(
            parse_tart_running_names(json).expect("names"),
            vec!["shipyard-build-runner-abc", "other"]
        );
    }

    #[test]
    fn parse_tart_running_names_accepts_running_boolean() {
        let json = r#"[
            {"Name":"a","Running":true},
            {"Name":"b","Running":false}
        ]"#;
        assert_eq!(parse_tart_running_names(json).expect("names"), vec!["a"]);
    }

    #[test]
    fn parse_tart_running_names_empty_list_is_empty() {
        assert!(parse_tart_running_names("[]").expect("names").is_empty());
        assert!(
            parse_tart_running_names("[\n\n]")
                .expect("names")
                .is_empty()
        );
    }

    #[test]
    fn parse_tart_running_names_errors_on_garbage_not_zero() {
        // Crucial: unparseable output must error (→ unreadable host, fail-closed),
        // never silently count as 0 free-advertising running VMs.
        assert!(parse_tart_running_names("").is_err());
        assert!(parse_tart_running_names("command not found: tart").is_err());
        assert!(parse_tart_running_names("{not an array}").is_err());
    }

    #[test]
    fn parse_tart_running_names_errors_when_running_vm_has_no_name() {
        let json = r#"[{"Running":true}]"#;
        assert!(parse_tart_running_names(json).is_err());
    }

    #[test]
    fn parse_tart_get_os_reads_os_field() {
        assert_eq!(
            parse_tart_get_os(r#"{"Name":"pulp-build-runner:latest","OS":"darwin"}"#).expect("os"),
            "darwin"
        );
        assert_eq!(
            parse_tart_get_os(r#"{"name":"linux","os":"linux"}"#).expect("os"),
            "linux"
        );
        assert!(parse_tart_get_os("{}").is_err());
    }

    #[test]
    fn is_macos_os_accepts_darwin_and_rejects_linux() {
        assert!(is_macos_os("darwin"));
        assert!(is_macos_os("macOS"));
        assert!(!is_macos_os("linux"));
    }

    #[test]
    fn free_slots_fail_closed_on_unreadable_host() {
        let readable = HostCapacity {
            class: "studio".to_owned(),
            ssh: None,
            cap: 2,
            running: Some(1),
            source: "local".to_owned(),
        };
        let unreadable = HostCapacity {
            class: "m1".to_owned(),
            ssh: Some("macpro".to_owned()),
            cap: 2,
            running: None,
            source: "ssh error: timed out".to_owned(),
        };
        assert_eq!(readable.free(), 1);
        assert_eq!(
            unreadable.free(),
            0,
            "unreadable host must not advertise capacity"
        );
        assert!(!unreadable.readable());
        assert_eq!(total_free(&[readable, unreadable]), 1);
    }

    #[test]
    fn free_slots_saturate_when_running_exceeds_cap() {
        // 3 VMs on a cap-2 host (e.g. Appendix-D override was reverted) → 0 free,
        // never a negative/underflowed huge number.
        let over = HostCapacity {
            class: "studio".to_owned(),
            ssh: None,
            cap: 2,
            running: Some(3),
            source: "local".to_owned(),
        };
        assert_eq!(over.free(), 0);
    }

    #[test]
    fn any_unreadable_flags_partial_reads() {
        let ok = HostCapacity {
            class: "studio".to_owned(),
            ssh: None,
            cap: 2,
            running: Some(0),
            source: "local".to_owned(),
        };
        let bad = HostCapacity {
            class: "m1".to_owned(),
            ssh: Some("macpro".to_owned()),
            cap: 2,
            running: None,
            source: "ssh error".to_owned(),
        };
        assert!(!any_unreadable(std::slice::from_ref(&ok)));
        assert!(any_unreadable(&[ok, bad]));
    }

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
            tartci_bin: "/Users/ci/.local/bin/tartci".to_owned(),
            tart_home: Some("/Users/ci user/VMs".to_owned()),
            labels: Vec::new(),
        };
        assert_eq!(
            remote_tart_command(&class, &["get", "vm one", "--format", "json"]),
            "env TART_HOME='/Users/ci user/VMs' /opt/homebrew/bin/tart get 'vm one' --format json"
        );
    }
}
