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

use chrono::{DateTime, Duration, Utc};
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
    classify_local_failures: bool,
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
    /// Seconds since the newest `JetsamEvent-*` report (null/absent = none).
    jetsam_age_s: Option<i64>,
    /// Seconds since the newest `WindowServer-*.ips` report (null/absent = none).
    windowserver_age_s: Option<i64>,
    /// Epoch seconds when the sample was taken. Preferred over the file mtime as
    /// the reference for reconstructing incident times, so a `touch`/copy that
    /// bumps mtime without refreshing the ages cannot drift the window. Absent in
    /// older producers → we fall back to the file mtime.
    sampled_at: Option<i64>,
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

/// Tolerance (seconds) for matching a reconstructed incident time against the
/// leg window. `jetsam_age_s` and the file mtime are both integer-second
/// precision, so a couple seconds absorbs rounding at the window boundaries.
/// Deliberately small — a broad grace could turn an unrelated code failure into
/// a masked "infra" label, which is the one direction we must not err toward.
const INCIDENT_WINDOW_TOLERANCE: Duration = Duration::seconds(2);

/// Resolve the opt-in incident-reclassification setting into a concrete vitals
/// path. `Some(path)` when `[host_health] classify_local_failures` is on;
/// `None` when off (the default). Resolved once at the command layer where
/// `LoadedConfig` is available, then threaded to the execution seam so the
/// per-target probe needs only a path — not the whole config.
#[must_use]
pub fn incident_reclassify_path(config: &LoadedConfig) -> Option<PathBuf> {
    let cfg = load_config(config);
    cfg.classify_local_failures.then(|| resolve_path(&cfg))
}

/// If the `host_vitals` signal at `path` shows a host infra incident (jetsam /
/// `WindowServer` crash) whose reconstructed time overlaps `[started_at,
/// completed_at]`, return a human reason for reclassifying a local TEST failure
/// as INFRA. Otherwise `None`.
///
/// FAILS OPEN: an absent/unreadable/stale signal, or no overlapping incident,
/// yields `None` (keep the original class). Only the caller — which knows the
/// leg was local + TEST — acts on this; see `crate::classify`.
#[must_use]
pub fn incident_from_path(
    path: &Path,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Option<String> {
    let vitals = read_vitals(path)?;
    // Prefer the producer's explicit sample timestamp; fall back to the file
    // mtime for older producers that don't emit one.
    let sample_time = vitals
        .sampled_at
        .and_then(|epoch| DateTime::<Utc>::from_timestamp(epoch, 0))
        .or_else(|| file_mtime_utc(path))?;
    incident_overlap(&vitals, sample_time, started_at, completed_at)
}

/// Pure overlap core, split out so tests drive it without touching the FS.
/// `sample_time` is when the signal was written (the vitals file mtime); each
/// incident's time is reconstructed as `sample_time - age_s`.
fn incident_overlap(
    vitals: &HostVitals,
    sample_time: DateTime<Utc>,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> Option<String> {
    for (age_s, label) in [
        (vitals.jetsam_age_s, "jetsam (memory-pressure kill)"),
        (vitals.windowserver_age_s, "WindowServer crash"),
    ] {
        let Some(age_s) = age_s else { continue };
        if age_s < 0 {
            continue;
        }
        // Checked throughout: `age_s` is an unbounded value from a foreign JSON
        // producer, so a garbage magnitude must fail open (skip), never panic.
        let (Some(delta), Some(low), Some(high)) = (
            Duration::try_seconds(age_s),
            started_at.checked_sub_signed(INCIDENT_WINDOW_TOLERANCE),
            completed_at.checked_add_signed(INCIDENT_WINDOW_TOLERANCE),
        ) else {
            continue;
        };
        let Some(incident_at) = sample_time.checked_sub_signed(delta) else {
            continue;
        };
        if incident_at >= low && incident_at <= high {
            return Some(format!(
                "host {label} at {} overlapped the validation window ({} – {})",
                incident_at.to_rfc3339(),
                started_at.to_rfc3339(),
                completed_at.to_rfc3339()
            ));
        }
    }
    None
}

fn file_mtime_utc(path: &Path) -> Option<DateTime<Utc>> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(gate: bool, block_on_critical: bool) -> HostHealthConfig {
        HostHealthConfig {
            gate,
            block_on_critical,
            ..Default::default()
        }
    }

    fn vitals(code: i64, reason: &str) -> HostVitals {
        HostVitals {
            code: Some(code),
            reason: Some(reason.to_owned()),
            ..Default::default()
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
            reason: Some("garbled".to_owned()),
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        match evaluate_with(&cfg(true, false), Some(raw)) {
            HostHealthOutcome::Warn(message) => assert!(message.contains("CRITICAL"), "{message}"),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn missing_reason_has_a_placeholder() {
        let raw = HostVitals {
            code: Some(10),
            ..Default::default()
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
        assert_eq!(parsed.jetsam_age_s, Some(23962));
        assert_eq!(parsed.windowserver_age_s, Some(23686));
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

    // ---- incident-overlap reclassification (Part 2) ----

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("timestamp")
    }

    fn vitals_ages(jetsam: Option<i64>, windowserver: Option<i64>) -> HostVitals {
        HostVitals {
            jetsam_age_s: jetsam,
            windowserver_age_s: windowserver,
            ..Default::default()
        }
    }

    #[test]
    fn jetsam_incident_inside_window_overlaps() {
        // sample at t=2000, jetsam 500s earlier → incident at 1500, inside [1000,2000].
        let out = incident_overlap(&vitals_ages(Some(500), None), ts(2000), ts(1000), ts(2000));
        let message = out.expect("overlap");
        assert!(message.contains("jetsam"), "{message}");
    }

    #[test]
    fn incident_before_window_does_not_overlap() {
        // incident at 2000-1500 = 500, well before the window start 1000.
        assert!(
            incident_overlap(&vitals_ages(Some(1500), None), ts(2000), ts(1000), ts(2000))
                .is_none()
        );
    }

    #[test]
    fn incident_after_window_does_not_overlap() {
        // sample at 2000, age 0 → incident at 2000, after window end 1200.
        assert!(
            incident_overlap(&vitals_ages(Some(0), None), ts(2000), ts(1000), ts(1200)).is_none()
        );
    }

    #[test]
    fn windowserver_incident_overlaps_and_is_labeled() {
        let out = incident_overlap(&vitals_ages(None, Some(300)), ts(1800), ts(1000), ts(2000));
        let message = out.expect("overlap");
        assert!(message.contains("WindowServer"), "{message}");
    }

    #[test]
    fn no_ages_never_overlaps() {
        assert!(incident_overlap(&vitals_ages(None, None), ts(2000), ts(1000), ts(2000)).is_none());
    }

    #[test]
    fn negative_age_is_ignored() {
        assert!(
            incident_overlap(&vitals_ages(Some(-5), None), ts(2000), ts(1000), ts(2000)).is_none()
        );
    }

    #[test]
    fn boundary_incident_within_tolerance_overlaps() {
        // incident at 999 is 1s before start 1000 — within the 2s rounding tolerance.
        assert!(
            incident_overlap(&vitals_ages(Some(1001), None), ts(2000), ts(1000), ts(2000))
                .is_some()
        );
    }

    #[test]
    fn absurd_age_fails_open_without_panicking() {
        // A garbage/foreign magnitude must skip (fail open), never panic the worker.
        assert!(
            incident_overlap(
                &vitals_ages(Some(i64::MAX), None),
                ts(2000),
                ts(1000),
                ts(2000)
            )
            .is_none()
        );
        assert!(
            incident_overlap(
                &vitals_ages(None, Some(i64::MAX)),
                ts(2000),
                ts(1000),
                ts(2000)
            )
            .is_none()
        );
    }

    #[test]
    fn incident_from_path_prefers_sampled_at_over_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        // sampled_at is an ancient epoch; jetsam 500s earlier → incident at 999_500.
        // The file's real mtime is ~now, so if the code used mtime this would never
        // fall in the ancient window below.
        let file = write_vitals_file(dir.path(), r#"{"sampled_at":1000000,"jetsam_age_s":500}"#);
        let overlap = incident_from_path(Path::new(&file), ts(999_000), ts(1_000_000));
        assert!(overlap.expect("overlap").contains("jetsam"));
        // A window that excludes the sampled_at-derived incident → None.
        assert!(incident_from_path(Path::new(&file), ts(2_000_000), ts(2_001_000)).is_none());
    }

    fn loaded_config(toml: &str) -> LoadedConfig {
        use crate::config::LocalOverlaySource;
        use toml::Table;
        LoadedConfig {
            data: toml.parse::<Table>().expect("toml"),
            global_dir: std::path::PathBuf::from("/tmp/global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn write_vitals_file(dir: &Path, body: &str) -> String {
        let path = dir.join("host_vitals.json");
        std::fs::write(&path, body).expect("write vitals");
        path.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn reclassify_path_is_none_when_gate_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = write_vitals_file(dir.path(), r#"{"jetsam_age_s":5}"#);
        let config = loaded_config(&format!(
            "[host_health]\nclassify_local_failures = false\nfile = \"{file}\"\n"
        ));
        assert!(incident_reclassify_path(&config).is_none());
    }

    #[test]
    fn reclassify_path_resolves_when_gate_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = write_vitals_file(dir.path(), r#"{"jetsam_age_s":5}"#);
        let config = loaded_config(&format!(
            "[host_health]\nclassify_local_failures = true\nfile = \"{file}\"\n"
        ));
        assert_eq!(
            incident_reclassify_path(&config),
            Some(PathBuf::from(&file))
        );
    }

    #[test]
    fn incident_from_path_overlaps_recent_jetsam() {
        let dir = tempfile::tempdir().expect("tempdir");
        // File mtime ≈ now; jetsam 5s ago → incident ≈ now-5s, inside a wide window.
        let file = write_vitals_file(dir.path(), r#"{"jetsam_age_s":5}"#);
        let now = Utc::now();
        let out = incident_from_path(
            Path::new(&file),
            now - Duration::hours(1),
            now + Duration::hours(1),
        );
        assert!(out.expect("overlap").contains("jetsam"));
    }

    #[test]
    fn incident_from_path_absent_file_fails_open() {
        let now = Utc::now();
        assert!(
            incident_from_path(
                Path::new("/nonexistent/host_vitals.json"),
                now - Duration::hours(1),
                now + Duration::hours(1),
            )
            .is_none()
        );
    }
}
