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
//! - **Bounded retries.** An attempt is recorded before it starts, a tag is
//!   attempted at most once per retry window, and after a fixed number of
//!   attempts (or at once, when the release is ineligible) the tag is terminal:
//!   it is never retried and an alert is raised.
//! - **Only lagging hosts, never a downgrade.** A rollout names exactly the
//!   host classes behind the release. A host *ahead* of the latest release
//!   stops the whole run with an alert rather than being downgraded.
//! - **One mutation at a time.** The controller lock spans the decision and the
//!   rollout; a tick that finds it held records nothing and exits.

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

/// Exit code when a host runs a newer version than the latest release.
pub(super) const EXIT_RECONCILE_AHEAD: u8 = 4;
/// Exit code when the latest release is terminal for this controller.
pub(super) const EXIT_RECONCILE_TERMINAL: u8 = 5;
/// Attempts per tag before it becomes terminal (the CLI default).
#[cfg(test)]
pub(super) const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// What the reconciler decided.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub(super) enum ReconcileDecision {
    /// Every host runs the latest release.
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
    /// This tag stopped being retried. Nothing more happens without an operator.
    Terminal {
        reason: String,
        lagging: Vec<String>,
    },
    /// At least one host runs a newer version than the latest release. Nothing
    /// is rolled out anywhere: "latest" is not what the fleet thinks it is.
    Ahead { hosts: Vec<String> },
    /// Hosts lag and a rollout is due, to exactly these host classes.
    Rollout { lagging: Vec<String> },
    /// The release or at least one host version could not be read.
    Unknown { reason: String },
    /// Another rollout holds the controller lock; nothing was read or recorded.
    ControllerBusy,
}

/// Attempts at one tag.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct TagAttempts {
    #[serde(default)]
    pub(super) attempts: u32,
    #[serde(default)]
    pub(super) last_attempt: Option<DateTime<Utc>>,
    /// Why the tag is no longer retried.
    #[serde(default)]
    pub(super) terminal: Option<String>,
}

/// Persisted attempt ledger, keyed by tag.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct AttemptLedger {
    #[serde(default)]
    pub(super) tags: BTreeMap<String, TagAttempts>,
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

fn write_ledger(state_dir: &Path, ledger: &AttemptLedger) -> Result<(), String> {
    let path = ledger_path(state_dir);
    let parent = path
        .parent()
        .ok_or_else(|| "fleet-reconcile ledger path has no parent".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create fleet-reconcile state dir: {error}"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("stage fleet-reconcile ledger: {error}"))?;
    serde_json::to_writer_pretty(&mut temp, ledger)
        .map_err(|error| format!("write fleet-reconcile ledger: {error}"))?;
    temp.persist(&path)
        .map_err(|error| format!("persist fleet-reconcile ledger: {error}"))?;
    Ok(())
}

/// Record an attempt atomically before the rollout starts, returning the
/// attempt count including this one.
pub(super) fn record_attempt(
    state_dir: &Path,
    tag: &str,
    at: DateTime<Utc>,
) -> Result<u32, String> {
    let mut ledger = read_ledger(state_dir)?;
    let entry = ledger.tags.entry(tag.to_owned()).or_default();
    entry.attempts += 1;
    entry.last_attempt = Some(at);
    let attempts = entry.attempts;
    write_ledger(state_dir, &ledger)?;
    Ok(attempts)
}

/// Stop retrying a tag.
pub(super) fn mark_terminal(state_dir: &Path, tag: &str, reason: &str) -> Result<(), String> {
    let mut ledger = read_ledger(state_dir)?;
    ledger.tags.entry(tag.to_owned()).or_default().terminal = Some(reason.to_owned());
    write_ledger(state_dir, &ledger)
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
#[derive(Clone, Copy, Debug)]
pub(super) struct ReconcilePolicy {
    pub(super) soak: chrono::Duration,
    pub(super) retry: chrono::Duration,
    pub(super) max_attempts: u32,
}

/// Decide what to do. Pure: every input is passed in.
pub(super) fn decide(
    latest: Result<&PublishedRelease, &str>,
    hosts: &[HostVersion],
    ledger: &AttemptLedger,
    now: DateTime<Utc>,
    policy: ReconcilePolicy,
) -> ReconcileDecision {
    let latest = match latest {
        Ok(latest) => latest,
        Err(reason) => {
            return ReconcileDecision::Unknown {
                reason: format!("latest published release could not be read: {reason}"),
            };
        }
    };
    let Some(target) = parse_version(&latest.tag) else {
        return ReconcileDecision::Unknown {
            reason: format!(
                "latest release tag {:?} is not vMAJOR.MINOR.PATCH",
                latest.tag
            ),
        };
    };
    if hosts.is_empty() {
        return ReconcileDecision::Unknown {
            reason: "no host classes are configured".to_owned(),
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
    if !unknown.is_empty() {
        return ReconcileDecision::Unknown {
            reason: format!("host version unreadable: {}", unknown.join(", ")),
        };
    }
    let ahead = hosts
        .iter()
        .filter(|host| {
            host.version
                .as_deref()
                .and_then(parse_version)
                .is_some_and(|installed| installed > target)
        })
        .map(|host| {
            format!(
                "{} ({})",
                host.host_class,
                host.version.as_deref().unwrap_or("?")
            )
        })
        .collect::<Vec<_>>();
    if !ahead.is_empty() {
        return ReconcileDecision::Ahead { hosts: ahead };
    }
    let lagging = hosts
        .iter()
        .filter(|host| host.lagging == Some(true))
        .map(|host| host.host_class.clone())
        .collect::<Vec<_>>();
    if lagging.is_empty() {
        return ReconcileDecision::UpToDate;
    }
    let attempts = ledger.tags.get(&latest.tag).cloned().unwrap_or_default();
    if let Some(reason) = attempts.terminal {
        return ReconcileDecision::Terminal { reason, lagging };
    }
    if attempts.attempts >= policy.max_attempts {
        return ReconcileDecision::Terminal {
            reason: format!("gave up after {} attempts", attempts.attempts),
            lagging,
        };
    }
    let soak_until = latest.published_at + policy.soak;
    if now < soak_until {
        return ReconcileDecision::Soaking {
            soak_until,
            lagging,
        };
    }
    if let Some(last_attempt) = attempts.last_attempt {
        let next_attempt = last_attempt + policy.retry;
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

/// Result of one rollout the reconciler started.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(super) enum RolloutOutcome {
    Verified,
    Failed {
        reason: String,
    },
    /// Refused before any host was touched; retrying cannot help.
    Ineligible {
        reason: String,
    },
}

/// The world the reconciler reads and acts on. Production talks to GitHub and
/// the hosts; tests substitute every edge.
pub(super) trait ReconcileEnv {
    fn now(&self) -> DateTime<Utc>;
    fn latest_release(&mut self) -> Result<PublishedRelease, String>;
    /// One entry per configured host class, in configuration order.
    fn probe_hosts(&mut self) -> Vec<HostVersion>;
    fn rollout(&mut self, tag: &str, host_classes: &[String]) -> RolloutOutcome;
    /// Open or refresh the operator alert for this condition.
    fn alert(&mut self, title: &str, body: &str) -> Result<(), String>;
}

/// Everything one reconcile tick observed and did.
#[derive(Clone, Debug, Serialize)]
pub(super) struct ReconcileReport {
    pub(super) latest_release: Option<PublishedRelease>,
    pub(super) hosts: Vec<HostVersion>,
    pub(super) decision: ReconcileDecision,
    pub(super) rollout: Option<RolloutOutcome>,
    pub(super) attempt: Option<u32>,
    pub(super) terminal: Option<String>,
    pub(super) alerts: Vec<String>,
    pub(super) exit_code: u8,
}

pub(super) fn alert_title(tag: &str) -> String {
    format!("fleet-reconcile: {tag} could not reach the fleet")
}

/// One reconcile tick.
pub(super) fn run_reconcile<E: ReconcileEnv>(
    env: &mut E,
    state_dir: &Path,
    policy: ReconcilePolicy,
    apply: bool,
) -> ReconcileReport {
    let mut report = ReconcileReport {
        latest_release: None,
        hosts: Vec::new(),
        decision: ReconcileDecision::ControllerBusy,
        rollout: None,
        attempt: None,
        terminal: None,
        alerts: Vec::new(),
        exit_code: super::EXIT_CONTROLLER_BUSY,
    };
    let _lock = match super::controller_lock::try_acquire(state_dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => return report,
        Err(reason) => {
            report.decision = ReconcileDecision::Unknown { reason };
            report.exit_code = EXIT_RECONCILE_UNKNOWN;
            return report;
        }
    };
    let now = env.now();
    let latest = env.latest_release();
    let mut hosts = env.probe_hosts();
    if let Ok(latest) = &latest {
        classify_hosts(latest, &mut hosts);
    }
    report.latest_release = latest.as_ref().ok().cloned();
    let decision = match read_ledger(state_dir) {
        Ok(ledger) => decide(
            latest.as_ref().map_err(String::as_str),
            &hosts,
            &ledger,
            now,
            policy,
        ),
        Err(reason) => ReconcileDecision::Unknown { reason },
    };
    report.hosts = hosts;
    report.decision = decision.clone();
    report.exit_code = match &decision {
        ReconcileDecision::UpToDate | ReconcileDecision::Soaking { .. } => 0,
        ReconcileDecision::Unknown { .. } => EXIT_RECONCILE_UNKNOWN,
        ReconcileDecision::RateLimited { .. } => EXIT_RECONCILE_RATE_LIMITED,
        ReconcileDecision::Terminal { .. } => EXIT_RECONCILE_TERMINAL,
        ReconcileDecision::ControllerBusy => super::EXIT_CONTROLLER_BUSY,
        ReconcileDecision::Ahead { hosts } => {
            let tag = report
                .latest_release
                .as_ref()
                .map_or("?", |latest| latest.tag.as_str())
                .to_owned();
            let body = format!(
                "Hosts run a newer Shipyard than the latest published release {tag}: {}. \
                 fleet-reconcile never downgrades, so it rolled nothing out. Check whether \
                 {tag} is really the release the fleet should run.",
                hosts.join(", ")
            );
            raise(
                env,
                &mut report,
                &format!("fleet-reconcile: hosts ahead of {tag}"),
                &body,
            );
            EXIT_RECONCILE_AHEAD
        }
        ReconcileDecision::Rollout { .. } if !apply => 0,
        ReconcileDecision::Rollout { lagging } => {
            let latest = latest.expect("a rollout decision requires a readable release");
            rollout(
                env,
                &mut report,
                state_dir,
                policy,
                &latest.tag,
                lagging,
                now,
            )
        }
    };
    report
}

fn raise<E: ReconcileEnv>(env: &mut E, report: &mut ReconcileReport, title: &str, body: &str) {
    match env.alert(title, body) {
        Ok(()) => report.alerts.push(title.to_owned()),
        Err(error) => report
            .alerts
            .push(format!("{title} (ALERT NOT DELIVERED: {error})")),
    }
}

fn rollout<E: ReconcileEnv>(
    env: &mut E,
    report: &mut ReconcileReport,
    state_dir: &Path,
    policy: ReconcilePolicy,
    tag: &str,
    lagging: &[String],
    now: DateTime<Utc>,
) -> u8 {
    // Record before mutating: a crash or a failing rollout still counts as an
    // attempt, so a broken release cannot loop every tick.
    let attempt = match record_attempt(state_dir, tag, now) {
        Ok(attempt) => attempt,
        Err(reason) => {
            report.decision = ReconcileDecision::Unknown { reason };
            return EXIT_RECONCILE_UNKNOWN;
        }
    };
    report.attempt = Some(attempt);
    let outcome = env.rollout(tag, lagging);
    report.rollout = Some(outcome.clone());
    let terminal = match &outcome {
        RolloutOutcome::Verified => return 0,
        RolloutOutcome::Ineligible { reason } => format!("release is ineligible: {reason}"),
        RolloutOutcome::Failed { reason } if attempt >= policy.max_attempts => {
            format!("gave up after {attempt} attempts; last failure: {reason}")
        }
        RolloutOutcome::Failed { .. } => return 1,
    };
    if let Err(error) = mark_terminal(state_dir, tag, &terminal) {
        report
            .alerts
            .push(format!("could not record terminal state: {error}"));
    }
    let body = format!(
        "fleet-reconcile stopped retrying {tag}: {terminal}.\n\nLagging host classes: {}.\n\n\
         Fix the release or the hosts, then run `shipyard runner fleet-update --to {tag} \
         --host-class <class> --apply` by hand.",
        lagging.join(", ")
    );
    raise(env, report, &alert_title(tag), &body);
    report.terminal = Some(terminal);
    EXIT_RECONCILE_TERMINAL
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

    fn policy() -> ReconcilePolicy {
        ReconcilePolicy {
            soak: chrono::Duration::minutes(30),
            retry: chrono::Duration::hours(6),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }

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
        decide(Ok(latest), &hosts, ledger, now, policy())
    }

    #[test]
    fn a_rollout_names_only_the_lagging_host_classes() {
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
    fn a_host_ahead_of_latest_stops_everything_and_is_never_downgraded() {
        let now = Utc::now();
        let latest = release("v0.208.0", 45, now);
        let decision = run(
            &latest,
            vec![host("m1", Some("0.205.0")), host("m5", Some("0.209.1"))],
            &AttemptLedger::default(),
            now,
        );
        assert_eq!(
            decision,
            ReconcileDecision::Ahead {
                hosts: vec!["m5 (0.209.1)".to_owned()]
            }
        );
    }

    #[test]
    fn current_hosts_are_up_to_date() {
        let now = Utc::now();
        let latest = release("v0.208.0", 45, now);
        let decision = run(
            &latest,
            vec![host("m1", Some("0.208.0")), host("m5", Some("0.208.0"))],
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
        assert!(matches!(
            decide(
                Err("HTTP 502"),
                &[],
                &AttemptLedger::default(),
                now,
                policy()
            ),
            ReconcileDecision::Unknown { .. }
        ));
        assert!(matches!(
            run(&latest, Vec::new(), &AttemptLedger::default(), now),
            ReconcileDecision::Unknown { .. }
        ));
        let garbage = release("latest", 120, now);
        assert!(matches!(
            run(
                &garbage,
                vec![host("m1", Some("0.1.0"))],
                &AttemptLedger::default(),
                now
            ),
            ReconcileDecision::Unknown { .. }
        ));
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
    fn attempt_ledger_counts_and_a_corrupt_one_fails_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let at = Utc::now();
        assert_eq!(
            record_attempt(temp.path(), "v0.208.0", at).expect("record"),
            1
        );
        assert_eq!(
            record_attempt(temp.path(), "v0.208.0", at).expect("record"),
            2
        );
        mark_terminal(temp.path(), "v0.208.0", "why").expect("terminal");
        let ledger = read_ledger(temp.path()).expect("read");
        let entry = &ledger.tags["v0.208.0"];
        assert_eq!(entry.attempts, 2);
        assert_eq!(entry.last_attempt, Some(at));
        assert_eq!(entry.terminal.as_deref(), Some("why"));
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

    // ------------------------------------------------------------------
    // Command-level: run_reconcile against a fake world.
    // ------------------------------------------------------------------

    struct FakeEnv {
        now: DateTime<Utc>,
        latest: Result<PublishedRelease, String>,
        hosts: Vec<(String, Option<String>)>,
        outcome: RolloutOutcome,
        rollouts: Vec<(String, Vec<String>)>,
        alerts: Vec<(String, String)>,
    }

    impl FakeEnv {
        fn new(now: DateTime<Utc>, hosts: &[(&str, &str)], outcome: RolloutOutcome) -> Self {
            Self {
                now,
                latest: Ok(release("v0.208.0", 120, now)),
                hosts: hosts
                    .iter()
                    .map(|(name, version)| ((*name).to_owned(), Some((*version).to_owned())))
                    .collect(),
                outcome,
                rollouts: Vec::new(),
                alerts: Vec::new(),
            }
        }
    }

    impl ReconcileEnv for FakeEnv {
        fn now(&self) -> DateTime<Utc> {
            self.now
        }
        fn latest_release(&mut self) -> Result<PublishedRelease, String> {
            self.latest.clone()
        }
        fn probe_hosts(&mut self) -> Vec<HostVersion> {
            self.hosts
                .iter()
                .map(|(name, version)| host(name, version.as_deref()))
                .collect()
        }
        fn rollout(&mut self, tag: &str, host_classes: &[String]) -> RolloutOutcome {
            self.rollouts.push((tag.to_owned(), host_classes.to_vec()));
            self.outcome.clone()
        }
        fn alert(&mut self, title: &str, body: &str) -> Result<(), String> {
            self.alerts.push((title.to_owned(), body.to_owned()));
            Ok(())
        }
    }

    fn failed() -> RolloutOutcome {
        RolloutOutcome::Failed {
            reason: "m5 failed post-rollout verification".to_owned(),
        }
    }

    #[test]
    fn a_failing_release_is_attempted_once_per_window_then_becomes_terminal() {
        let temp = tempfile::tempdir().expect("temp");
        let start = Utc::now();
        let mut env = FakeEnv::new(start, &[("m1", "0.208.0"), ("m5", "0.205.0")], failed());

        let first = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(first.exit_code, 1);
        assert_eq!(first.attempt, Some(1));
        // The same tick again: the recorded attempt must hold the rollout off.
        let again = run_reconcile(&mut env, temp.path(), policy(), true);
        assert!(
            matches!(again.decision, ReconcileDecision::RateLimited { .. }),
            "{:?}",
            again.decision
        );
        assert_eq!(again.exit_code, EXIT_RECONCILE_RATE_LIMITED);
        assert_eq!(
            env.rollouts.len(),
            1,
            "a recorded attempt must stop a second rollout"
        );

        env.now = start + chrono::Duration::hours(7);
        assert_eq!(
            run_reconcile(&mut env, temp.path(), policy(), true).attempt,
            Some(2)
        );
        assert!(env.alerts.is_empty(), "no alert before the cap");
        env.now = start + chrono::Duration::hours(14);
        let third = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(third.exit_code, EXIT_RECONCILE_TERMINAL);
        assert!(
            third
                .terminal
                .as_deref()
                .unwrap_or_default()
                .contains("gave up after 3 attempts")
        );
        assert_eq!(env.alerts.len(), 1);
        assert_eq!(env.alerts[0].0, alert_title("v0.208.0"));
        assert!(env.alerts[0].1.contains("Lagging host classes: m5"));

        env.now = start + chrono::Duration::days(3);
        let after = run_reconcile(&mut env, temp.path(), policy(), true);
        assert!(matches!(after.decision, ReconcileDecision::Terminal { .. }));
        assert_eq!(env.rollouts.len(), 3, "a terminal tag is never retried");
        assert_eq!(
            env.alerts.len(),
            1,
            "a terminal tag is not re-alerted every tick"
        );
        // Every rollout named only the lagging class.
        assert!(
            env.rollouts
                .iter()
                .all(|(tag, classes)| tag == "v0.208.0" && classes == &["m5"])
        );
    }

    #[test]
    fn an_ineligible_release_is_terminal_at_once() {
        let temp = tempfile::tempdir().expect("temp");
        let now = Utc::now();
        let mut env = FakeEnv::new(
            now,
            &[("m5", "0.205.0")],
            RolloutOutcome::Ineligible {
                reason: "missing build-provenance attestation".to_owned(),
            },
        );
        let report = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(report.exit_code, EXIT_RECONCILE_TERMINAL);
        assert_eq!(report.attempt, Some(1));
        assert_eq!(env.alerts.len(), 1);
        assert!(env.alerts[0].1.contains("release is ineligible"));
        env.now = now + chrono::Duration::days(1);
        run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(env.rollouts.len(), 1);
    }

    #[test]
    fn a_verified_rollout_needs_no_alert_and_later_ticks_are_up_to_date() {
        let temp = tempfile::tempdir().expect("temp");
        let now = Utc::now();
        let mut env = FakeEnv::new(now, &[("m5", "0.205.0")], RolloutOutcome::Verified);
        assert_eq!(
            run_reconcile(&mut env, temp.path(), policy(), true).exit_code,
            0
        );
        env.hosts[0].1 = Some("0.208.0".to_owned());
        let later = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(later.decision, ReconcileDecision::UpToDate);
        assert!(env.alerts.is_empty());
    }

    #[test]
    fn a_host_ahead_alerts_and_rolls_nothing_out() {
        let temp = tempfile::tempdir().expect("temp");
        let mut env = FakeEnv::new(
            Utc::now(),
            &[("m1", "0.205.0"), ("m5", "0.210.0")],
            RolloutOutcome::Verified,
        );
        let report = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(report.exit_code, EXIT_RECONCILE_AHEAD);
        assert!(
            env.rollouts.is_empty(),
            "never downgrade, never roll the rest either"
        );
        assert_eq!(env.alerts.len(), 1);
        assert!(env.alerts[0].1.contains("m5 (0.210.0)"));
    }

    #[test]
    fn report_only_mode_decides_but_records_and_mutates_nothing() {
        let temp = tempfile::tempdir().expect("temp");
        let mut env = FakeEnv::new(Utc::now(), &[("m5", "0.205.0")], RolloutOutcome::Verified);
        let report = run_reconcile(&mut env, temp.path(), policy(), false);
        assert!(matches!(report.decision, ReconcileDecision::Rollout { .. }));
        assert_eq!(report.exit_code, 0);
        assert!(env.rollouts.is_empty());
        assert!(read_ledger(temp.path()).expect("ledger").tags.is_empty());
    }

    #[test]
    fn a_held_controller_lock_skips_the_tick_without_recording() {
        let temp = tempfile::tempdir().expect("temp");
        let held = super::super::controller_lock::try_acquire(temp.path())
            .expect("lock")
            .expect("free");
        let mut env = FakeEnv::new(Utc::now(), &[("m5", "0.205.0")], RolloutOutcome::Verified);
        let report = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(report.decision, ReconcileDecision::ControllerBusy);
        assert_eq!(report.exit_code, super::super::EXIT_CONTROLLER_BUSY);
        assert!(env.rollouts.is_empty());
        assert!(
            !ledger_path(temp.path()).exists(),
            "a skipped tick records nothing"
        );
        drop(held);
        assert_eq!(
            run_reconcile(&mut env, temp.path(), policy(), true).exit_code,
            0
        );
    }

    /// Adding a machine is adding a host class: nothing is named in code.
    #[test]
    fn a_fourth_host_class_is_enumerated_and_rolled_like_any_other() {
        let temp = tempfile::tempdir().expect("temp");
        let mut env = FakeEnv::new(
            Utc::now(),
            &[
                ("m1", "0.208.0"),
                ("m5", "0.208.0"),
                ("studio", "0.208.0"),
                ("m7-new-rack", "0.205.0"),
            ],
            RolloutOutcome::Verified,
        );
        let report = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(report.hosts.len(), 4);
        assert_eq!(
            env.rollouts,
            vec![("v0.208.0".to_owned(), vec!["m7-new-rack".to_owned()])]
        );
    }
}
