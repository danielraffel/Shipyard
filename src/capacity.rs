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
//! from `tart list` on each host — the Studio also hosts long-lived runner
//! agents and ephemeral builders that consume its slots, so the live count is
//! the truth, not a static assumption.
//!
//! **Fail-closed:** a host whose VM state can't be read (SSH/`tart` error,
//! unparseable output) contributes `free = 0` and is flagged unreadable — it is
//! never counted as spare capacity. Silence must not read as success.
//!
//! This module is the pure logic: config parsing, `tart list` parsing, and the
//! free-slot math. SSH'ing each host and shelling `tart` is the impure edge in
//! the CLI handler.

use serde::Deserialize;
use toml::{Table, Value as TomlValue};

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
    /// Routing/pin labels this host class's runners carry (informational; the
    /// reroute watcher uses `<repo>-build-<class>` to target this host).
    pub labels: Vec<String>,
}

/// Parse `[host_class]` from merged config. Returns classes sorted by name for
/// stable output. An empty/absent section yields an empty vec.
///
/// # Errors
/// Returns a human-readable message when a class entry is malformed (not a
/// table, non-string `ssh`/`tart_bin`, non-integer/negative `cap`, or a
/// `labels` that is not an array of strings).
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
            labels,
        });
    }
    out.sort_by(|a, b| a.class.cmp(&b.class));
    Ok(out)
}

/// One VM entry from `tart list --format json`. Tart has used both a
/// `"State": "running"` string and a `"Running": true` boolean across versions,
/// so we accept either and ignore everything else.
#[derive(Debug, Clone, Deserialize)]
struct TartVm {
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

/// Count running VMs in `tart list --format json` output.
///
/// # Errors
/// Returns a message when the output is not the expected JSON array — the
/// caller treats that as an unreadable host (fail-closed), never as zero
/// running VMs (which would falsely advertise free capacity).
pub fn parse_tart_running(json: &str) -> Result<u32, String> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Err("empty `tart list` output".to_owned());
    }
    let vms: Vec<TartVm> = serde_json::from_str(trimmed)
        .map_err(|error| format!("could not parse `tart list --format json`: {error}"))?;
    let running = vms.iter().filter(|vm| vm.is_running()).count();
    u32::try_from(running).map_err(|_| "implausibly many running VMs".to_owned())
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
            ssh = "Daniels-Mac-Studio.local"
            cap = 4
            labels = ["self-hosted", "shipyard-build-studio"]

            [host_class.m1]
            ssh = "Daniels-MacBook-Pro.local"
            "#,
        );
        let classes = parse_host_classes(&cfg).expect("parse");
        assert_eq!(classes.len(), 2);
        // sorted by name: m1 before studio
        assert_eq!(classes[0].class, "m1");
        assert_eq!(classes[0].cap, DEFAULT_CAP);
        assert_eq!(classes[0].tart_bin, "tart");
        assert_eq!(classes[1].class, "studio");
        assert_eq!(classes[1].cap, 4);
        assert_eq!(classes[1].ssh.as_deref(), Some("Daniels-Mac-Studio.local"));
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
    fn parse_tart_running_counts_running_state_string() {
        let json = r#"[
            {"Source":"local","Name":"shipyard-build-runner-abc","State":"running"},
            {"Source":"local","Name":"golden","State":"stopped"},
            {"Source":"local","Name":"other","State":"Running"}
        ]"#;
        assert_eq!(parse_tart_running(json).expect("count"), 2);
    }

    #[test]
    fn parse_tart_running_counts_running_boolean() {
        let json = r#"[
            {"Name":"a","Running":true},
            {"Name":"b","Running":false}
        ]"#;
        assert_eq!(parse_tart_running(json).expect("count"), 1);
    }

    #[test]
    fn parse_tart_running_empty_list_is_zero() {
        assert_eq!(parse_tart_running("[]").expect("count"), 0);
        assert_eq!(parse_tart_running("[\n\n]").expect("count"), 0);
    }

    #[test]
    fn parse_tart_running_errors_on_garbage_not_zero() {
        // Crucial: unparseable output must error (→ unreadable host, fail-closed),
        // never silently count as 0 free-advertising running VMs.
        assert!(parse_tart_running("").is_err());
        assert!(parse_tart_running("command not found: tart").is_err());
        assert!(parse_tart_running("{not an array}").is_err());
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
}
