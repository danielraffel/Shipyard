//! Optional host-health awareness for pre-dispatch preflight.
//!
//! Reads the shared `host_vitals` signal (Pulp's `tools/scripts/host_vitals.sh`
//! plus its launchd sensor, or any producer emitting the same JSON contract) so
//! an operator whose self-hosted runner is *co-located with heavy interactive
//! work* can surface — or, opt-in, hard-stop on — a saturated host BEFORE a ship
//! runs into a memory-pressure / jetsam / reboot failure. Motivated by a
//! downstream incident where an over-subscribed runner rebooted mid-job and
//! failed the required gate for an infra reason, not the code.
//!
//! Everything here is **OFF by default** and **FAILS OPEN**: absent config,
//! absent signal file, or an unreadable/garbled file all yield "no opinion" (no
//! warning, no block). A broken probe must never wedge a ship — the worst case
//! is we forgo avoidance we cannot measure. This is the deliberate inverse of
//! backend-reachability preflight, which fails closed: reachability gates
//! correctness, host-health gates only crash-avoidance.
//!
//! ## Signal contract
//!
//! A JSON object with (at least) a numeric `code` (0 green / 10 warn / 20
//! critical) and/or a string `level` (`green` / `warn` / `critical`), plus an
//! optional human `reason`. `code` wins when both are present. Any other shape
//! is treated as "no opinion".
//!
//! ## Config (`[host_health]` in `config.toml`, all default OFF)
//!
//! - `gate` (bool) — master opt-in. When false the signal is never read.
//! - `block_on_critical` (bool) — escalate a `critical` reading from a soft
//!   warning to a hard preflight failure (non-zero exit).
//! - `file` (string) — override the signal path. Default is the launchd
//!   sensor's location, `~/.local/state/pulp/host_vitals.json`. The
//!   `SHIPYARD_HOST_VITALS_FILE` env var overrides both (used by tests).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::LoadedConfig;
use crate::paths::home_dir;

/// Env override for the signal path (highest precedence; primarily for tests).
pub const HOST_VITALS_FILE_ENV: &str = "SHIPYARD_HOST_VITALS_FILE";

/// Health level parsed from the `host_vitals` signal (green < warn < critical).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HostHealthLevel {
    /// Host is healthy.
    Green,
    /// Host is under elevated load / early memory pressure.
    Warn,
    /// Host is saturated (memory-pressure critical / recent jetsam).
    Critical,
}

impl HostHealthLevel {
    fn from_code(code: i64) -> Option<Self> {
        match code {
            0 => Some(Self::Green),
            10 => Some(Self::Warn),
            20 => Some(Self::Critical),
            _ => None,
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        match label {
            "green" => Some(Self::Green),
            "warn" => Some(Self::Warn),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

/// The result of consulting the host-health signal before dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostHealthOutcome {
    /// Nothing to surface: gate off, signal absent/unreadable, or green.
    Ok,
    /// Surface a soft warning; the ship proceeds.
    Warn(String),
    /// Block the ship: host is `critical` and `block_on_critical` is set.
    Block {
        /// Level label for the error message (always `critical` today).
        level: String,
        /// Human reason from the signal.
        reason: String,
    },
}

/// Raw `[host_health]` config sub-table. `#[serde(default)]` makes every field
/// (and the whole block) optional, so absence is the off-by-default state.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct HostHealthConfig {
    gate: bool,
    block_on_critical: bool,
    file: Option<String>,
}

/// Raw `host_vitals` signal. Every field optional so a partial/foreign producer
/// never fails to parse — we simply report "no opinion" when we can't classify.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct HostVitals {
    level: Option<String>,
    code: Option<i64>,
    reason: Option<String>,
}

impl HostVitals {
    /// Classify the level. Numeric `code` is the primary contract; the `level`
    /// string is a fallback. Returns `None` when neither is recognizable.
    fn level(&self) -> Option<HostHealthLevel> {
        if let Some(code) = self.code
            && let Some(level) = HostHealthLevel::from_code(code)
        {
            return Some(level);
        }
        self.level.as_deref().and_then(HostHealthLevel::from_label)
    }

    fn reason(&self) -> String {
        self.reason
            .clone()
            .unwrap_or_else(|| "no reason reported".to_owned())
    }
}

fn load_config(config: &LoadedConfig) -> HostHealthConfig {
    config
        .get("host_health")
        .and_then(|value| value.clone().try_into().ok())
        .unwrap_or_default()
}

fn default_vitals_path() -> PathBuf {
    home_dir()
        .join(".local")
        .join("state")
        .join("pulp")
        .join("host_vitals.json")
}

fn resolve_path(cfg: &HostHealthConfig) -> PathBuf {
    if let Some(path) = std::env::var_os(HOST_VITALS_FILE_ENV) {
        return PathBuf::from(path);
    }
    if let Some(file) = &cfg.file {
        return PathBuf::from(file);
    }
    default_vitals_path()
}

/// Read + parse the signal file. Missing or malformed → `None` (fail open).
fn read_vitals(path: &Path) -> Option<HostVitals> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Consult the host-health signal for a pre-dispatch decision. Off by default;
/// fails open on any absent/unreadable input.
#[must_use]
pub fn evaluate(config: &LoadedConfig) -> HostHealthOutcome {
    let cfg = load_config(config);
    evaluate_with(&cfg, read_vitals(&resolve_path(&cfg)))
}

/// Pure decision core, split out so tests drive it without touching the FS.
fn evaluate_with(cfg: &HostHealthConfig, vitals: Option<HostVitals>) -> HostHealthOutcome {
    if !cfg.gate {
        return HostHealthOutcome::Ok;
    }
    let Some(vitals) = vitals else {
        return HostHealthOutcome::Ok; // signal absent → no opinion (fail open)
    };
    let Some(level) = vitals.level() else {
        return HostHealthOutcome::Ok; // unclassifiable → no opinion (fail open)
    };
    let reason = vitals.reason();
    match level {
        HostHealthLevel::Green => HostHealthOutcome::Ok,
        HostHealthLevel::Warn => HostHealthOutcome::Warn(format!(
            "Host-health WARN before dispatch: {reason}. The self-hosted runner is under load; \
             prefer shipping via GitHub-native auto-merge over a foreground watch."
        )),
        HostHealthLevel::Critical => {
            if cfg.block_on_critical {
                HostHealthOutcome::Block {
                    level: "critical".to_owned(),
                    reason,
                }
            } else {
                HostHealthOutcome::Warn(format!(
                    "Host-health CRITICAL before dispatch: {reason}. The self-hosted runner is \
                     saturated (memory pressure / recent jetsam); a validation failure here is \
                     likely infra, not your code. Set host_health.block_on_critical to hard-stop, \
                     or ship via GitHub-native auto-merge."
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(gate: bool, block_on_critical: bool) -> HostHealthConfig {
        HostHealthConfig {
            gate,
            block_on_critical,
            file: None,
        }
    }

    fn vitals(code: i64, reason: &str) -> HostVitals {
        HostVitals {
            level: None,
            code: Some(code),
            reason: Some(reason.to_owned()),
        }
    }

    #[test]
    fn gate_off_is_always_ok_even_when_critical() {
        let out = evaluate_with(&cfg(false, true), Some(vitals(20, "saturated")));
        assert_eq!(out, HostHealthOutcome::Ok);
    }

    #[test]
    fn absent_signal_fails_open() {
        let out = evaluate_with(&cfg(true, true), None);
        assert_eq!(out, HostHealthOutcome::Ok);
    }

    #[test]
    fn unclassifiable_signal_fails_open() {
        // No code, unknown level string → cannot classify → no opinion.
        let raw = HostVitals {
            level: Some("bananas".to_owned()),
            code: None,
            reason: Some("garbled".to_owned()),
        };
        assert_eq!(
            evaluate_with(&cfg(true, true), Some(raw)),
            HostHealthOutcome::Ok
        );
    }

    #[test]
    fn green_is_ok() {
        assert_eq!(
            evaluate_with(&cfg(true, false), Some(vitals(0, "healthy"))),
            HostHealthOutcome::Ok
        );
    }

    #[test]
    fn warn_surfaces_a_soft_warning() {
        let out = evaluate_with(&cfg(true, false), Some(vitals(10, "load 124 > 3x28 cores")));
        match out {
            HostHealthOutcome::Warn(message) => {
                assert!(message.contains("WARN"), "{message}");
                assert!(message.contains("load 124"), "{message}");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn critical_warns_by_default_does_not_block() {
        let out = evaluate_with(
            &cfg(true, false),
            Some(vitals(20, "memory pressure critical")),
        );
        match out {
            HostHealthOutcome::Warn(message) => {
                assert!(message.contains("CRITICAL"), "{message}");
                assert!(message.contains("block_on_critical"), "{message}");
            }
            other => panic!("expected Warn (default), got {other:?}"),
        }
    }

    #[test]
    fn critical_blocks_when_opted_in() {
        let out = evaluate_with(&cfg(true, true), Some(vitals(20, "jetsam 30s ago")));
        assert_eq!(
            out,
            HostHealthOutcome::Block {
                level: "critical".to_owned(),
                reason: "jetsam 30s ago".to_owned(),
            }
        );
    }

    #[test]
    fn code_wins_over_level_string() {
        // Numeric contract is authoritative: code=20 critical even if the string
        // disagrees. Guards against a producer whose label drifted from its code.
        let raw = HostVitals {
            level: Some("green".to_owned()),
            code: Some(20),
            reason: Some("saturated".to_owned()),
        };
        assert_eq!(
            evaluate_with(&cfg(true, true), Some(raw)),
            HostHealthOutcome::Block {
                level: "critical".to_owned(),
                reason: "saturated".to_owned(),
            }
        );
    }

    #[test]
    fn level_string_used_when_code_absent() {
        let raw = HostVitals {
            level: Some("critical".to_owned()),
            code: None,
            reason: None,
        };
        match evaluate_with(&cfg(true, false), Some(raw)) {
            HostHealthOutcome::Warn(message) => assert!(message.contains("CRITICAL"), "{message}"),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn missing_reason_has_a_placeholder() {
        let raw = HostVitals {
            level: None,
            code: Some(10),
            reason: None,
        };
        match evaluate_with(&cfg(true, false), Some(raw)) {
            HostHealthOutcome::Warn(message) => {
                assert!(message.contains("no reason reported"), "{message}");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn read_vitals_parses_the_sensor_contract() {
        // The exact JSON shape the launchd sensor publishes.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("host_vitals.json");
        std::fs::write(
            &path,
            r#"{"level":"warn","code":10,"reason":"load 124.14 > 3x28 cores","os":"Darwin","ncpu":28,"load1":"124.14","pressure_level":"1","jetsam_age_s":23962,"windowserver_age_s":23686}"#,
        )
        .expect("write");
        let parsed = read_vitals(&path).expect("parse");
        assert_eq!(parsed.level(), Some(HostHealthLevel::Warn));
        assert_eq!(parsed.reason(), "load 124.14 > 3x28 cores");
    }

    #[test]
    fn read_vitals_missing_file_is_none() {
        assert!(read_vitals(Path::new("/nonexistent/host_vitals.json")).is_none());
    }

    #[test]
    fn read_vitals_garbage_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("host_vitals.json");
        std::fs::write(&path, "not json {{{").expect("write");
        assert!(read_vitals(&path).is_none());
    }
}
