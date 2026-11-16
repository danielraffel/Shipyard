//! What the fleet-update rollout shares with `shipyard doctor --fleet`: the
//! published-release and per-host version probes, and the persisted ledger of
//! hosts a failed rollback left quarantined.

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

/// Exit code when a release or host version could not be read.
pub(super) const EXIT_FLEET_UNKNOWN: u8 = 9;

const PROBE_CONTROLLER_MARKER: &str = "SHIPYARD_PROBE_DECLARES_HOST_CLASSES=";

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
    /// A remote host whose own machine-global config declares
    /// `[host_class.*]`: a second fleet controller. `None` for the controller
    /// itself or when unread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) declares_host_classes: Option<bool>,
}

/// What a failed rollback left on one tag.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct TagRecord {
    /// Why the tag is no longer rolled automatically.
    #[serde(default)]
    pub(super) terminal: Option<String>,
}

/// Persisted rollout ledger, keyed by tag.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct RolloutLedger {
    #[serde(default)]
    pub(super) tags: BTreeMap<String, TagRecord>,
    /// Never rolled automatically again until an operator clears it.
    #[serde(default)]
    pub(super) quarantined: BTreeMap<String, Quarantine>,
}

/// A host that was mutated, failed, and could not be restored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Quarantine {
    pub(super) tag: String,
    pub(super) reason: String,
    pub(super) since: DateTime<Utc>,
}

/// Read, change and atomically rewrite the ledger. A change that leaves the
/// ledger as it was writes nothing: the ledger lives in the protected
/// production state tree, and an idle tick must not touch it.
pub(super) fn update_ledger<F: FnOnce(&mut RolloutLedger)>(
    state_dir: &Path,
    change: F,
) -> Result<(), String> {
    let before = read_ledger(state_dir)?;
    let mut ledger = before.clone();
    change(&mut ledger);
    if ledger == before {
        return Ok(());
    }
    write_ledger(state_dir, &ledger)
}

/// Why the host's own install guard is held right now, if it is: a Sandbox
/// canary (or an install) owns this host, so a rollout tick must defer before
/// what made its tag terminal, that tag becomes eligible again too.
pub(super) fn clear_host(state_dir: &Path, host_class: &str) -> Result<Option<Quarantine>, String> {
    let mut cleared = None;
    update_ledger(state_dir, |ledger| {
        cleared = ledger.quarantined.remove(host_class);
        if let Some(quarantine) = &cleared
            && let Some(attempts) = ledger.tags.get_mut(&quarantine.tag)
            && attempts
                .terminal
                .as_deref()
                .is_some_and(|reason| reason.starts_with(&rollback_failed_prefix(host_class)))
        {
            *attempts = TagRecord::default();
        }
    })?;
    Ok(cleared)
}

/// Record a host that was mutated and could not be restored: quarantine it and
/// make the tag terminal, under the caller's controller lock. Returns the
/// terminal reason and the alert body.
pub(super) fn record_rollback_failure(
    state_dir: &Path,
    tag: &str,
    host_class: &str,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(String, String), String> {
    let terminal = format!("{}{reason}", rollback_failed_prefix(host_class));
    update_ledger(state_dir, |ledger| {
        ledger.quarantined.insert(
            host_class.to_owned(),
            Quarantine {
                tag: tag.to_owned(),
                reason: reason.to_owned(),
                since: now,
            },
        );
        ledger.tags.entry(tag.to_owned()).or_default().terminal = Some(terminal.clone());
    })?;
    Ok((terminal, rollback_failure_body(tag, host_class, reason)))
}

pub(super) fn alert_title(tag: &str) -> String {
    format!("fleet-reconcile: {tag} could not reach the fleet")
}

pub(super) fn rollback_failure_body(tag: &str, host_class: &str, reason: &str) -> String {
    format!(
        "A fleet rollout of {tag} updated {host_class}, the host failed, and restoring its \
         previous version ALSO failed: {reason}.\n\n{host_class} needs an operator. It is \
         quarantined: no automatic rollout will touch it until \
         `shipyard runner fleet-update --clear-host {host_class}` is run after it is fixed."
    )
}

fn rollback_failed_prefix(host_class: &str) -> String {
    format!("rollback failed on {host_class}: ")
}

/// The directory keeps its original name so ledgers written by earlier
/// releases, including any quarantine, stay readable.
pub(super) fn ledger_path(state_dir: &Path) -> PathBuf {
    state_dir.join("fleet-reconcile").join("attempts.json")
}

/// Read the attempt ledger. A missing file is an empty ledger; a corrupt one is
/// an error, because silently forgetting attempts would re-enable the loop.
pub(super) fn read_ledger(state_dir: &Path) -> Result<RolloutLedger, String> {
    match std::fs::read_to_string(ledger_path(state_dir)) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|error| format!("fleet rollout ledger is corrupt: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RolloutLedger::default()),
        Err(error) => Err(format!("cannot read fleet rollout ledger: {error}")),
    }
}

fn write_ledger(state_dir: &Path, ledger: &RolloutLedger) -> Result<(), String> {
    let path = ledger_path(state_dir);
    let parent = path
        .parent()
        .ok_or_else(|| "fleet rollout ledger path has no parent".to_owned())?;
    // The ledger is production persistence in the protected state tree: like
    // every other such write it holds the shared writer-domain lease, so it
    // waits for (and then defers to) an exclusive Sandbox contamination audit
    // instead of landing inside one.
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)
        .map_err(|error| format!("fleet rollout ledger writer domain: {error}"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create fleet rollout state dir: {error}"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("stage fleet rollout ledger: {error}"))?;
    serde_json::to_writer_pretty(&mut temp, ledger)
        .map_err(|error| format!("write fleet rollout ledger: {error}"))?;
    temp.persist(&path)
        .map_err(|error| format!("persist fleet rollout ledger: {error}"))?;
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

/// Tuning for [`decide`].
/// Read one configured host's installed version. See [`probe_version_at`].
pub(super) fn probe_host_version(class: &HostClassConfig) -> HostVersion {
    probe_version_at(
        &class.class,
        class.ssh.as_deref(),
        class.shipyard_bin.as_deref(),
        class.shipyard_global_dir.as_deref(),
    )
}

/// Ask one configured host whether its pr-watch passes still complete
/// (`shipyard --json pr-watch liveness`), read-only. The command exits 1 when
/// the host is stale, so the JSON is read whatever the exit status. A host
/// whose Shipyard predates the subcommand answers with an error.
pub(super) fn probe_pr_watch_liveness(
    class: &HostClassConfig,
) -> Result<crate::pr_watch::liveness::Liveness, String> {
    let Some(binary) = class
        .shipyard_bin
        .as_deref()
        .filter(|path| path.starts_with('/'))
    else {
        return Err("host_class has no absolute shipyard_bin".to_owned());
    };
    let script = format!("{} --json pr-watch liveness", shlex_quote(binary));
    let mut command = if let Some(host) = class.ssh.as_deref() {
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
    let label = format!("pr-watch liveness probe for host class {}", class.class);
    let output =
        crate::process::run_output_until(&mut command, Instant::now() + HOST_PROBE_TIMEOUT, &label)
            .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.trim()).map_err(|_| {
        format!(
            "no pr-watch liveness answer (exit {}); its Shipyard may predate `pr-watch liveness`",
            output.status.code().unwrap_or(-1)
        )
    })
}

/// Read one host's installed `shipyard --version` without mutating anything.
/// For a remote host with a known global dir, also report whether its own
/// machine-global config declares host classes (a second fleet controller).
pub(super) fn probe_version_at(
    class: &str,
    ssh: Option<&str>,
    binary: Option<&str>,
    global_dir: Option<&str>,
) -> HostVersion {
    let mut result = HostVersion {
        host_class: class.to_owned(),
        ssh: ssh.map(ToOwned::to_owned),
        version: None,
        error: None,
        lagging: None,
        declares_host_classes: None,
    };
    let Some(binary) = binary.filter(|path| path.starts_with('/')) else {
        result.error = Some("host_class has no absolute shipyard_bin".to_owned());
        return result;
    };
    let mut script = format!("{} --version", shlex_quote(binary));
    if ssh.is_some()
        && let Some(global_dir) = global_dir.filter(|path| path.starts_with('/'))
    {
        let config = shlex_quote(&format!("{global_dir}/config.toml"));
        let marker = shlex_quote(PROBE_CONTROLLER_MARKER);
        let probe = format!(
            "\nif /usr/bin/grep -q '^\\[host_class\\.' {config} 2>/dev/null; then printf '%s1\\n' {marker}; else printf '%s0\\n' {marker}; fi"
        );
        script.push_str(&probe);
    }
    let mut command = if let Some(host) = ssh {
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
    let label = format!("version probe for host class {class}");
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
            result.declares_host_classes = text
                .lines()
                .find_map(|line| line.strip_prefix(PROBE_CONTROLLER_MARKER))
                .map(|value| value.trim() == "1");
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
    fn a_corrupt_ledger_fails_closed_and_an_unchanged_one_is_not_written() {
        let temp = tempfile::tempdir().expect("temp");
        update_ledger(temp.path(), |_| {}).expect("idle");
        assert!(
            !ledger_path(temp.path()).exists(),
            "a change that changes nothing writes nothing"
        );
        update_ledger(temp.path(), |ledger| {
            ledger
                .tags
                .entry("v0.208.0".to_owned())
                .or_default()
                .terminal = Some("why".to_owned());
        })
        .expect("write");
        let ledger = read_ledger(temp.path()).expect("read");
        assert_eq!(ledger.tags["v0.208.0"].terminal.as_deref(), Some("why"));
        std::fs::write(ledger_path(temp.path()), "not json").expect("corrupt");
        assert!(read_ledger(temp.path()).is_err());
        assert!(update_ledger(temp.path(), |_| {}).is_err());
    }

    #[test]
    fn clearing_a_host_reopens_the_tag_its_failed_rollback_closed() {
        let temp = tempfile::tempdir().expect("temp");
        record_rollback_failure(temp.path(), "v0.208.0", "m5", "daemon down", Utc::now())
            .expect("record");
        record_rollback_failure(temp.path(), "v0.207.0", "m1", "other", Utc::now())
            .expect("record");
        let cleared = clear_host(temp.path(), "m5")
            .expect("clear")
            .expect("was quarantined");
        assert_eq!(cleared.tag, "v0.208.0");
        let ledger = read_ledger(temp.path()).expect("read");
        assert!(!ledger.quarantined.contains_key("m5"));
        assert!(ledger.quarantined.contains_key("m1"));
        assert!(ledger.tags["v0.208.0"].terminal.is_none());
        assert!(ledger.tags["v0.207.0"].terminal.is_some());
        assert!(clear_host(temp.path(), "m5").expect("again").is_none());
    }

    #[test]
    fn hosts_are_classified_against_the_latest_release() {
        let latest = PublishedRelease {
            tag: "v0.208.0".to_owned(),
            published_at: Utc::now(),
        };
        let host = |version: Option<&str>| HostVersion {
            host_class: "m1".to_owned(),
            ssh: None,
            version: version.map(ToOwned::to_owned),
            error: None,
            lagging: None,
            declares_host_classes: None,
        };
        let mut hosts = [
            host(Some("0.205.0")),
            host(Some("0.208.0")),
            host(Some("0.210.0")),
            host(None),
        ];
        classify_hosts(&latest, &mut hosts);
        let lagging = hosts.iter().map(|host| host.lagging).collect::<Vec<_>>();
        assert_eq!(lagging, [Some(true), Some(false), Some(false), None]);
    }

    #[cfg(unix)]
    #[test]
    fn local_version_probe_reads_the_installed_binary() {
        let temp = tempfile::tempdir().expect("temp");
        let binary = temp.path().join("shipyard");
        crate::test_support::write_executable_script(
            &binary,
            "#!/bin/sh\n[ \"$1\" = --version ] && echo 'shipyard 0.205.0'\n",
        );
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
