//! `shipyard runner fleet-reconcile`: the backstop that brings the fleet up to
//! the latest published release when the release itself did not.
//!
//! A release cut with the local release script rolls itself out. One published
//! any other way (CI signing, a manual upload, an opted-out run) does not, and
//! nothing noticed until an operator did. This compares the latest published,
//! non-draft release with every configured host's installed version and, once
//! the release has soaked, runs the same verified `fleet-update` rollout.
//!
//! Two properties matter more than convenience:
//!
//! - **Fail closed on unknown.** A release or host version that cannot be read
//!   is reported as UNKNOWN (exit 9) and nothing is rolled out. Unreadable is
//!   never read as "up to date".
//! - **Bounded retries.** An attempt is recorded before it starts, and a tag is
//!   attempted at most once per retry window, so a release that cannot verify
//!   does not loop against the fleet every tick.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capacity::HostClassConfig;
use crate::executor::ssh::shlex_quote;
use crate::paths::{home_dir, unattended_tool_path};

/// Exit code when any input could not be read.
pub(super) const EXIT_RECONCILE_UNKNOWN: u8 = 9;
/// Exit code when hosts lag but the tag was attempted inside the retry window.
pub(super) const EXIT_RECONCILE_RATE_LIMITED: u8 = 3;

const HOST_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// The latest published release, as the release source reported it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct PublishedRelease {
    pub(super) tag: String,
    pub(super) published_at: DateTime<Utc>,
}

/// One host's installed version, or why it could not be read.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct HostVersion {
    pub(super) host_class: String,
    pub(super) ssh: Option<String>,
    pub(super) version: Option<String>,
    pub(super) error: Option<String>,
    pub(super) lagging: Option<bool>,
}

/// What the reconciler decided.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub(super) enum ReconcileDecision {
    /// Every host runs the latest release or newer.
    UpToDate,
    /// Hosts lag, but the release is still inside its soak window.
    Soaking {
        soak_until: DateTime<Utc>,
        lagging: Vec<String>,
    },
    /// Hosts lag and this tag was already attempted inside the retry window.
    RateLimited {
        last_attempt: DateTime<Utc>,
        next_attempt: DateTime<Utc>,
        lagging: Vec<String>,
    },
    /// Hosts lag and a rollout is due.
    Rollout { lagging: Vec<String> },
    /// The release or at least one host version could not be read.
    Unknown { reason: String },
}

/// Persisted attempt ledger, keyed by tag.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct AttemptLedger {
    #[serde(default)]
    pub(super) attempts: BTreeMap<String, DateTime<Utc>>,
}

pub(super) fn ledger_path(state_dir: &Path) -> PathBuf {
    state_dir.join("fleet-reconcile").join("attempts.json")
}

/// Read the attempt ledger. A missing file is an empty ledger; a corrupt one is
/// an error, because silently forgetting attempts would re-enable the loop.
pub(super) fn read_ledger(state_dir: &Path) -> Result<AttemptLedger, String> {
    match std::fs::read_to_string(ledger_path(state_dir)) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|error| format!("fleet-reconcile attempt ledger is corrupt: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AttemptLedger::default()),
        Err(error) => Err(format!(
            "cannot read fleet-reconcile attempt ledger: {error}"
        )),
    }
}

/// Record an attempt atomically before the rollout starts.
pub(super) fn record_attempt(state_dir: &Path, tag: &str, at: DateTime<Utc>) -> Result<(), String> {
    let path = ledger_path(state_dir);
    let mut ledger = read_ledger(state_dir)?;
    ledger.attempts.insert(tag.to_owned(), at);
    let parent = path
        .parent()
        .ok_or_else(|| "fleet-reconcile ledger path has no parent".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create fleet-reconcile state dir: {error}"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("stage fleet-reconcile ledger: {error}"))?;
    serde_json::to_writer_pretty(&mut temp, &ledger)
        .map_err(|error| format!("write fleet-reconcile ledger: {error}"))?;
    temp.persist(&path)
        .map_err(|error| format!("persist fleet-reconcile ledger: {error}"))?;
    Ok(())
}

pub(super) fn parse_version(raw: &str) -> Option<[u64; 3]> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("shipyard ").unwrap_or(raw);
    let raw = raw.strip_prefix('v').unwrap_or(raw);
    let parts = raw
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    <[u64; 3]>::try_from(parts).ok()
}

/// Mark each readable host as lagging or not against `latest`.
pub(super) fn classify_hosts(latest: &PublishedRelease, hosts: &mut [HostVersion]) {
    let target = parse_version(&latest.tag);
    for host in hosts.iter_mut() {
        host.lagging = match (host.version.as_deref().and_then(parse_version), target) {
            (Some(installed), Some(target)) => Some(installed < target),
            _ => None,
        };
    }
}

/// Decide what to do. Pure: every input is passed in.
pub(super) fn decide(
    latest: Result<&PublishedRelease, &str>,
    hosts: &[HostVersion],
    ledger: &AttemptLedger,
    now: DateTime<Utc>,
    soak: chrono::Duration,
    retry: chrono::Duration,
) -> ReconcileDecision {
    let latest = match latest {
        Ok(latest) => latest,
        Err(reason) => {
            return ReconcileDecision::Unknown {
                reason: format!("latest published release could not be read: {reason}"),
            };
        }
    };
    if parse_version(&latest.tag).is_none() {
        return ReconcileDecision::Unknown {
            reason: format!(
                "latest release tag {:?} is not vMAJOR.MINOR.PATCH",
                latest.tag
            ),
        };
    }
    let unknown = hosts
        .iter()
        .filter(|host| host.lagging.is_none())
        .map(|host| {
            format!(
                "{} ({})",
                host.host_class,
                host.error.as_deref().unwrap_or("version unreadable")
            )
        })
        .collect::<Vec<_>>();
    if hosts.is_empty() {
        return ReconcileDecision::Unknown {
            reason: "no host classes are configured".to_owned(),
        };
    }
    if !unknown.is_empty() {
        return ReconcileDecision::Unknown {
            reason: format!("host version unreadable: {}", unknown.join(", ")),
        };
    }
    let lagging = hosts
        .iter()
        .filter(|host| host.lagging == Some(true))
        .map(|host| host.host_class.clone())
        .collect::<Vec<_>>();
    if lagging.is_empty() {
        return ReconcileDecision::UpToDate;
    }
    let soak_until = latest.published_at + soak;
    if now < soak_until {
        return ReconcileDecision::Soaking {
            soak_until,
            lagging,
        };
    }
    if let Some(last_attempt) = ledger.attempts.get(&latest.tag).copied() {
        let next_attempt = last_attempt + retry;
        if now < next_attempt {
            return ReconcileDecision::RateLimited {
                last_attempt,
                next_attempt,
                lagging,
            };
        }
    }
    ReconcileDecision::Rollout { lagging }
}

/// Read one host's installed `shipyard --version` without mutating anything.
pub(super) fn probe_host_version(class: &HostClassConfig) -> HostVersion {
    let mut result = HostVersion {
        host_class: class.class.clone(),
        ssh: class.ssh.clone(),
        version: None,
        error: None,
        lagging: None,
    };
    let Some(binary) = class
        .shipyard_bin
        .as_deref()
        .filter(|path| path.starts_with('/'))
    else {
        result.error = Some("host_class has no absolute shipyard_bin".to_owned());
        return result;
    };
    let script = format!("{} --version", shlex_quote(binary));
    let mut command = if let Some(host) = &class.ssh {
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
    let label = format!("version probe for host class {}", class.class);
    match crate::process::run_output_until(
        &mut command,
        Instant::now() + HOST_PROBE_TIMEOUT,
        &label,
    ) {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            let line = text.lines().next().unwrap_or_default().trim().to_owned();
            if parse_version(&line).is_some() {
                result.version = Some(line.trim_start_matches("shipyard ").to_owned());
            } else {
                result.error = Some(format!("unparseable version output {line:?}"));
            }
        }
        Ok(output) => {
            result.error = Some(format!(
                "version probe exited {}",
                output.status.code().unwrap_or(-1)
            ));
        }
        Err(error) => result.error = Some(error.to_string()),
    }
    result
}

/// Parse `GET repos/<repo>/releases/latest`, which already excludes drafts and
/// prereleases. A draft or prerelease that slipped through is refused anyway.
pub(super) fn parse_latest_release(value: &Value) -> Result<PublishedRelease, String> {
    if value.get("draft").and_then(Value::as_bool) != Some(false)
        || value.get("prerelease").and_then(Value::as_bool) != Some(false)
    {
        return Err("latest release is not a published, non-draft, non-prerelease".to_owned());
    }
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| "latest release has no tag_name".to_owned())?
        .to_owned();
    let published_at = value
        .get("published_at")
        .and_then(Value::as_str)
        .ok_or_else(|| "latest release has no published_at".to_owned())?;
    let published_at = DateTime::parse_from_rfc3339(published_at)
        .map_err(|error| format!("latest release published_at is malformed: {error}"))?
        .with_timezone(&Utc);
    Ok(PublishedRelease { tag, published_at })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, minutes_ago: i64, now: DateTime<Utc>) -> PublishedRelease {
        PublishedRelease {
            tag: tag.to_owned(),
            published_at: now - chrono::Duration::minutes(minutes_ago),
        }
    }

    fn host(name: &str, version: Option<&str>) -> HostVersion {
        HostVersion {
            host_class: name.to_owned(),
            ssh: None,
            version: version.map(ToOwned::to_owned),
            error: version
                .is_none()
                .then(|| "ssh: connect timed out".to_owned()),
            lagging: None,
        }
    }

    fn run(
        latest: &PublishedRelease,
        mut hosts: Vec<HostVersion>,
        ledger: &AttemptLedger,
        now: DateTime<Utc>,
    ) -> ReconcileDecision {
        classify_hosts(latest, &mut hosts);
        decide(
            Ok(latest),
            &hosts,
            ledger,
            now,
            chrono::Duration::minutes(30),
            chrono::Duration::hours(6),
        )
    }

    #[test]
    fn a_lagging_host_after_the_soak_triggers_a_rollout() {
        let now = Utc::now();
        let latest = release("v0.208.0", 45, now);
        let decision = run(
            &latest,
            vec![host("m1", Some("0.208.0")), host("m5", Some("0.205.0"))],
            &AttemptLedger::default(),
            now,
        );
        assert_eq!(
            decision,
            ReconcileDecision::Rollout {
                lagging: vec!["m5".to_owned()]
            }
        );
    }

    #[test]
    fn current_or_newer_hosts_are_up_to_date() {
        let now = Utc::now();
        let latest = release("v0.208.0", 45, now);
        let decision = run(
            &latest,
            vec![host("m1", Some("0.208.0")), host("m5", Some("0.209.1"))],
            &AttemptLedger::default(),
            now,
        );
        assert_eq!(decision, ReconcileDecision::UpToDate);
    }

    #[test]
    fn a_fresh_release_soaks_before_any_rollout() {
        let now = Utc::now();
        let latest = release("v0.208.0", 10, now);
        let decision = run(
            &latest,
            vec![host("m5", Some("0.205.0"))],
            &AttemptLedger::default(),
            now,
        );
        assert!(
            matches!(decision, ReconcileDecision::Soaking { ref lagging, .. } if lagging == &["m5"]),
            "{decision:?}"
        );
    }

    #[test]
    fn a_recent_attempt_rate_limits_the_same_tag_but_not_a_newer_one() {
        let now = Utc::now();
        let latest = release("v0.208.0", 120, now);
        let mut ledger = AttemptLedger::default();
        ledger
            .attempts
            .insert("v0.208.0".to_owned(), now - chrono::Duration::hours(1));
        let decision = run(&latest, vec![host("m5", Some("0.205.0"))], &ledger, now);
        assert!(
            matches!(decision, ReconcileDecision::RateLimited { .. }),
            "{decision:?}"
        );

        ledger
            .attempts
            .insert("v0.208.0".to_owned(), now - chrono::Duration::hours(7));
        let decision = run(&latest, vec![host("m5", Some("0.205.0"))], &ledger, now);
        assert!(
            matches!(decision, ReconcileDecision::Rollout { .. }),
            "{decision:?}"
        );

        ledger
            .attempts
            .insert("v0.208.0".to_owned(), now - chrono::Duration::minutes(5));
        let newer = release("v0.209.0", 60, now);
        let decision = run(&newer, vec![host("m5", Some("0.205.0"))], &ledger, now);
        assert!(
            matches!(decision, ReconcileDecision::Rollout { .. }),
            "{decision:?}"
        );
    }

    #[test]
    fn unreadable_inputs_are_unknown_never_up_to_date() {
        let now = Utc::now();
        let latest = release("v0.208.0", 120, now);
        let decision = run(
            &latest,
            vec![host("m1", Some("0.208.0")), host("m5", None)],
            &AttemptLedger::default(),
            now,
        );
        assert!(
            matches!(decision, ReconcileDecision::Unknown { ref reason } if reason.contains("m5 (ssh: connect timed out)")),
            "{decision:?}"
        );
        let decision = decide(
            Err("HTTP 502"),
            &[],
            &AttemptLedger::default(),
            now,
            chrono::Duration::minutes(30),
            chrono::Duration::hours(6),
        );
        assert!(matches!(decision, ReconcileDecision::Unknown { .. }));
        let decision = run(&latest, Vec::new(), &AttemptLedger::default(), now);
        assert!(matches!(decision, ReconcileDecision::Unknown { .. }));
        let garbage = release("latest", 120, now);
        let decision = run(
            &garbage,
            vec![host("m1", Some("0.1.0"))],
            &AttemptLedger::default(),
            now,
        );
        assert!(matches!(decision, ReconcileDecision::Unknown { .. }));
    }

    #[test]
    fn latest_release_parser_refuses_drafts_and_prereleases() {
        let good = serde_json::json!({
            "tag_name": "v0.208.0", "draft": false, "prerelease": false,
            "published_at": "2026-09-23T01:00:00Z"
        });
        assert_eq!(
            parse_latest_release(&good).expect("release").tag,
            "v0.208.0"
        );
        for field in ["draft", "prerelease"] {
            let mut bad = good.clone();
            bad[field] = Value::Bool(true);
            assert!(parse_latest_release(&bad).is_err(), "{field}");
        }
        let mut missing = good;
        missing
            .as_object_mut()
            .expect("object")
            .remove("published_at");
        assert!(parse_latest_release(&missing).is_err());
    }

    #[test]
    fn attempt_ledger_round_trips_and_a_corrupt_one_fails_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let at = Utc::now();
        record_attempt(temp.path(), "v0.208.0", at).expect("record");
        let ledger = read_ledger(temp.path()).expect("read");
        assert_eq!(ledger.attempts.get("v0.208.0"), Some(&at));
        std::fs::write(ledger_path(temp.path()), "not json").expect("corrupt");
        assert!(read_ledger(temp.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn local_version_probe_reads_the_installed_binary() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("temp");
        let binary = temp.path().join("shipyard");
        std::fs::write(
            &binary,
            "#!/bin/sh\n[ \"$1\" = --version ] && echo 'shipyard 0.205.0'\n",
        )
        .expect("fake");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let mut class = crate::capacity::HostClassConfig {
            class: "studio".to_owned(),
            ssh: None,
            cap: 2,
            tart_bin: "/opt/homebrew/bin/tart".to_owned(),
            tartci_bin: "/x/tartci".to_owned(),
            shipyard_bin: Some(binary.display().to_string()),
            shipyard_mode: None,
            shipyard_global_dir: None,
            shipyard_state_dir: None,
            github_cli: None,
            github_token_helper: None,
            tart_home: None,
            labels: Vec::new(),
        };
        let probed = probe_host_version(&class);
        assert_eq!(probed.version.as_deref(), Some("0.205.0"), "{probed:?}");
        class.shipyard_bin = Some("relative/shipyard".to_owned());
        assert!(probe_host_version(&class).error.is_some());
    }
}
