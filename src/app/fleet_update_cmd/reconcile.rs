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

use std::collections::{BTreeMap, BTreeSet};
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
    /// A remote host whose own machine-global config declares
    /// `[host_class.*]`: a second fleet controller. `None` for the controller
    /// itself or when unread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) declares_host_classes: Option<bool>,
    /// The host's daemon and its launchd launcher, when the probe could read
    /// them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) daemon: Option<HostDaemon>,
}

/// Whether a host's Shipyard daemon is running and will come back after a
/// reboot. Each field is `None` when its probe printed nothing readable.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(super) struct HostDaemon {
    pub(super) launcher_installed: Option<bool>,
    pub(super) launcher_active: Option<bool>,
    /// What the agent plist launchd loads says: it starts at login and
    /// restarts a daemon that exits non-zero. `None` when the host's Shipyard
    /// predates the field, or the plist is absent or unreadable.
    pub(super) survives_kill_and_reboot: Option<bool>,
    pub(super) running: Option<bool>,
}

impl HostDaemon {
    /// What is wrong with this host's daemon and the step that fixes it, or
    /// `None` when nothing read shows a problem.
    pub(super) fn problem(&self) -> Option<String> {
        let launcher = match (self.launcher_installed, self.launcher_active) {
            (Some(false), _) => Some(
                "the daemon launcher is not installed, so nothing restarts the daemon after a \
                 reboot; run `shipyard daemon launcher install` at the host's console (it needs \
                 a one-time macOS approval)",
            ),
            (Some(true), Some(false)) => Some(
                "the daemon launcher is installed but inactive; rerun `shipyard daemon launcher \
                 install` at the host's console",
            ),
            // An active launcher is only reboot-safe when the plist launchd
            // loads says so. An unread answer is not a yes.
            (Some(true), Some(true)) => match self.survives_kill_and_reboot {
                Some(true) => None,
                Some(false) => Some(
                    "the daemon's launchd agent neither starts at login nor restarts a daemon \
                     that dies (its plist has RunAtLoad or KeepAlive off); run `shipyard daemon \
                     refresh` on the host to rewrite it",
                ),
                None => Some(
                    "the daemon's launchd agent plist could not be read, so nothing shows the \
                     daemon comes back after a reboot or a kill; update the host's Shipyard and \
                     run `shipyard daemon refresh` there",
                ),
            },
            _ => None,
        };
        let stopped = (self.running == Some(false))
            .then_some("the daemon is not running; `shipyard daemon refresh` starts it");
        match (stopped, launcher) {
            (None, None) => None,
            (Some(stopped), None) => Some(stopped.to_owned()),
            (None, Some(launcher)) => Some(launcher.to_owned()),
            (Some(stopped), Some(launcher)) => Some(format!("{stopped}; {launcher}")),
        }
    }
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
    Rollout { tag: String, lagging: Vec<String> },
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
    /// How the last attempt ended (`verified`, or `failed: <reason>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_outcome: Option<String>,
}

/// Persisted attempt ledger, keyed by tag.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct AttemptLedger {
    #[serde(default)]
    pub(super) tags: BTreeMap<String, TagAttempts>,
    /// Never rolled automatically again until an operator clears it.
    #[serde(default)]
    pub(super) quarantined: BTreeMap<String, Quarantine>,
    /// Tags whose "hosts ahead" alert was already raised.
    #[serde(default)]
    pub(super) ahead_alerted: BTreeSet<String>,
    /// Consecutive ticks each host class could not be read.
    #[serde(default)]
    pub(super) unreachable_ticks: BTreeMap<String, u32>,
    /// Host classes whose "unreachable" alert was already raised.
    #[serde(default)]
    pub(super) unreachable_alerted: BTreeSet<String>,
    /// Host classes whose daemon alert was already raised; cleared when the
    /// host's daemon reads healthy again.
    #[serde(default)]
    pub(super) daemon_alerted: BTreeSet<String>,
    /// Host classes whose lag alert was already raised; cleared when the host
    /// catches up with the latest release.
    #[serde(default)]
    pub(super) lag_alerted: BTreeSet<String>,
}

/// A host that was mutated, failed, and could not be restored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Quarantine {
    pub(super) tag: String,
    pub(super) reason: String,
    pub(super) since: DateTime<Utc>,
}

/// Consecutive unreadable ticks before a host raises an alert (about an hour
/// at the 15-minute agent interval).
pub(super) const UNREACHABLE_ALERT_TICKS: u32 = 4;

/// Read, change and atomically rewrite the ledger. A change that leaves the
/// ledger as it was writes nothing: the ledger lives in the protected
/// production state tree, and an idle tick must not touch it.
pub(super) fn update_ledger<F: FnOnce(&mut AttemptLedger)>(
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
/// it records anything. Probes without creating or modifying the guard.
pub(super) fn local_install_guard_held(state_dir: &Path) -> Result<Option<String>, String> {
    let path = state_dir.join(super::auth_support::INSTALL_GUARD_NAME);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            Ok(None)
        }
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(Some(format!(
                "this host's install guard {} is held (a Sandbox canary or an install owns the \
                 host); the tick recorded nothing",
                path.display()
            )))
        }
        Err(error) => Err(format!("probe {}: {error}", path.display())),
    }
}

/// Clear an operator-resolved quarantine. When the host's failed rollback is
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
            *attempts = TagAttempts::default();
        }
    })?;
    Ok(cleared)
}

/// Record a host that was mutated and could not be restored: quarantine it and
/// make the tag terminal, under the caller's controller lock. Shared by
/// fleet-reconcile and by `fleet-update --apply` (the release stage), so every
/// path that can leave a broken host records it the same way. Returns the
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

pub(super) fn rollback_failure_body(tag: &str, host_class: &str, reason: &str) -> String {
    format!(
        "A fleet rollout of {tag} updated {host_class}, the host failed, and restoring its \
         previous version ALSO failed: {reason}.\n\n{host_class} needs an operator. It is \
         quarantined: no automatic rollout will touch it until \
         `shipyard runner fleet-reconcile --clear-host {host_class}` is run after it is fixed."
    )
}

fn rollback_failed_prefix(host_class: &str) -> String {
    format!("rollback failed on {host_class}: ")
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
    // The ledger is production persistence in the protected state tree: like
    // every other such write it holds the shared writer-domain lease, so it
    // waits for (and then defers to) an exclusive Sandbox contamination audit
    // instead of landing inside one.
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)
        .map_err(|error| format!("fleet-reconcile ledger writer domain: {error}"))?;
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

/// Undo the attempt [`record_attempt`] wrote for a rollout that deferred
/// before touching any host, restoring the tag's previous record. Refuses when
/// the record is no longer the one this tick wrote.
pub(super) fn withdraw_attempt(
    state_dir: &Path,
    tag: &str,
    attempt: u32,
    at: DateTime<Utc>,
    prior: Option<TagAttempts>,
) -> Result<(), String> {
    let mut ledger = read_ledger(state_dir)?;
    let current = ledger.tags.get(tag);
    if current.map(|entry| (entry.attempts, entry.last_attempt)) != Some((attempt, Some(at))) {
        return Err(format!(
            "attempt record for {tag} changed since it was written; left as is"
        ));
    }
    match prior {
        Some(prior) => {
            ledger.tags.insert(tag.to_owned(), prior);
        }
        None => {
            ledger.tags.remove(tag);
        }
    }
    write_ledger(state_dir, &ledger)
}

/// Record how the attempt at `tag` ended, so a ledger entry says whether the
/// rollout verified or failed instead of only that it was tried.
pub(super) fn record_outcome(state_dir: &Path, tag: &str, outcome: &str) -> Result<(), String> {
    update_ledger(state_dir, |ledger| {
        ledger.tags.entry(tag.to_owned()).or_default().last_outcome = Some(outcome.to_owned());
    })
}

/// Close every tag older than `latest` that never reached a verified rollout.
/// Reconcile only ever targets the latest release, so a failed attempt at an
/// older tag is never retried; left open it reads as a rollout in progress.
pub(super) fn retire_superseded(state_dir: &Path, latest: &str) -> Result<(), String> {
    let Some(latest_version) = parse_version(latest) else {
        return Ok(());
    };
    update_ledger(state_dir, |ledger| {
        for (tag, entry) in &mut ledger.tags {
            let older = parse_version(tag).is_some_and(|version| version < latest_version);
            let verified = entry.last_outcome.as_deref() == Some("verified");
            if older && !verified && entry.terminal.is_none() {
                let last = entry
                    .last_outcome
                    .as_deref()
                    .unwrap_or("outcome not recorded");
                entry.terminal = Some(format!(
                    "superseded by {latest} without a verified rollout (last attempt: {last})"
                ));
            }
        }
    })
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
    /// How long a host may lag a published release before it alerts.
    pub(super) lag_alert: chrono::Duration,
}

/// The newest release of `releases` (by version) that has itself soaked by
/// `now`. A release published later never makes an older one wait longer, and
/// an unsoaked release is never chosen, whatever soaked before it.
pub(super) fn newest_soaked(
    releases: &[PublishedRelease],
    now: DateTime<Utc>,
    soak: chrono::Duration,
) -> Option<&PublishedRelease> {
    releases
        .iter()
        .filter(|release| release.published_at + soak <= now)
        .filter_map(|release| parse_version(&release.tag).map(|version| (version, release)))
        .max_by_key(|(version, _)| *version)
        .map(|(_, release)| release)
}

/// When the earliest release newer than `version` that has not yet soaked
/// will have soaked.
fn next_soak_end(
    releases: &[PublishedRelease],
    newer_than: [u64; 3],
    now: DateTime<Utc>,
    soak: chrono::Duration,
) -> Option<DateTime<Utc>> {
    releases
        .iter()
        .filter(|release| parse_version(&release.tag).is_some_and(|version| version > newer_than))
        .map(|release| release.published_at + soak)
        .filter(|end| *end > now)
        .min()
}

/// The newest release that has itself soaked and the host classes behind it,
/// or the `Soaking` decision when no such release has a lagging host. The
/// latest release is one of `releases`, so a fleet with only that one behaves
/// as a single-release soak.
fn soaked_target(
    latest: &PublishedRelease,
    releases: &[PublishedRelease],
    reachable: &[&HostVersion],
    lagging: Vec<String>,
    ledger: &AttemptLedger,
    now: DateTime<Utc>,
    policy: ReconcilePolicy,
) -> Result<(PublishedRelease, Vec<String>), ReconcileDecision> {
    let mut candidates = releases.to_vec();
    if !candidates.iter().any(|release| release.tag == latest.tag) {
        candidates.push(latest.clone());
    }
    let soaking = |lagging: Vec<String>, newer_than: [u64; 3]| ReconcileDecision::Soaking {
        soak_until: next_soak_end(&candidates, newer_than, now, policy.soak)
            .unwrap_or(latest.published_at + policy.soak),
        lagging,
    };
    let Some(release) = newest_soaked(&candidates, now, policy.soak) else {
        return Err(soaking(lagging, [0, 0, 0]));
    };
    let Some(release_version) = parse_version(&release.tag) else {
        return Err(soaking(lagging, [0, 0, 0]));
    };
    let behind_release = reachable
        .iter()
        .filter(|host| {
            host.version
                .as_deref()
                .and_then(parse_version)
                .is_some_and(|installed| installed < release_version)
        })
        .filter(|host| !ledger.quarantined.contains_key(&host.host_class))
        .map(|host| host.host_class.clone())
        .collect::<Vec<_>>();
    if behind_release.is_empty() {
        // Every host already runs the newest soaked release; the newer ones
        // are still soaking.
        return Err(soaking(lagging, release_version));
    }
    Ok((release.clone(), behind_release))
}

/// Decide what to do. Pure: every input is passed in.
pub(super) fn decide(
    latest: Result<&PublishedRelease, &str>,
    releases: &[PublishedRelease],
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
    // An unreadable host is left out of this tick, not treated as current:
    // the reachable hosts are still reconciled, and the caller reports the
    // unreadable ones (and alerts once they stay unreadable).
    let reachable = hosts
        .iter()
        .filter(|host| host.lagging.is_some())
        .collect::<Vec<_>>();
    if reachable.is_empty() {
        return ReconcileDecision::Unknown {
            reason: format!(
                "no host version could be read: {}",
                hosts
                    .iter()
                    .map(|host| format!(
                        "{} ({})",
                        host.host_class,
                        host.error.as_deref().unwrap_or("version unreadable")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    let ahead = reachable
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
    let lagging = reachable
        .iter()
        .filter(|host| host.lagging == Some(true))
        .filter(|host| !ledger.quarantined.contains_key(&host.host_class))
        .map(|host| host.host_class.clone())
        .collect::<Vec<_>>();
    if lagging.is_empty() {
        return ReconcileDecision::UpToDate;
    }
    let (release, lagging) =
        match soaked_target(latest, releases, &reachable, lagging, ledger, now, policy) {
            Ok(target) => target,
            Err(decision) => return decision,
        };
    let attempts = ledger.tags.get(&release.tag).cloned().unwrap_or_default();
    if let Some(reason) = attempts.terminal {
        return ReconcileDecision::Terminal { reason, lagging };
    }
    if attempts.attempts >= policy.max_attempts {
        return ReconcileDecision::Terminal {
            reason: format!("gave up after {} attempts", attempts.attempts),
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
    ReconcileDecision::Rollout {
        tag: release.tag,
        lagging,
    }
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
    /// A host was mutated and could not be restored. It needs an operator.
    RollbackFailed {
        host_class: String,
        reason: String,
    },
    /// A host refused at its install guard before any change (a sandbox
    /// canary or another update owns it). Not an attempt: a later tick
    /// resumes the rollout without waiting out the retry window.
    Deferred {
        reason: String,
    },
}

/// The world the reconciler reads and acts on. Production talks to GitHub and
/// the hosts; tests substitute every edge.
pub(super) trait ReconcileEnv {
    fn now(&self) -> DateTime<Utc>;
    fn latest_release(&mut self) -> Result<PublishedRelease, String>;
    /// Recently published releases, newest or not, for picking the newest one
    /// that has itself soaked. Defaults to the latest release alone.
    fn recent_releases(&mut self) -> Result<Vec<PublishedRelease>, String> {
        self.latest_release().map(|latest| vec![latest])
    }
    /// One entry per configured host class, in configuration order.
    fn probe_hosts(&mut self) -> Vec<HostVersion>;
    fn rollout(&mut self, tag: &str, host_classes: &[String]) -> RolloutOutcome;
    /// Open or refresh the operator alert for this condition.
    fn alert(&mut self, title: &str, body: &str) -> Result<(), String>;
    /// `Some(reason)` when this controller host is owned by a Sandbox canary
    /// or an install, so a rollout tick must defer without recording.
    fn local_host_busy(&mut self) -> Result<Option<String>, String> {
        Ok(None)
    }
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
    /// Host classes whose version could not be read this tick.
    pub(super) unreachable: Vec<String>,
    /// Host classes that will not be rolled until an operator clears them.
    pub(super) quarantined: Vec<String>,
    /// Remote hosts whose own config declares `[host_class.*]`: there must be
    /// exactly one fleet controller.
    pub(super) other_controllers: Vec<String>,
    /// `<host class>: <problem and fix>` for each host whose daemon is not
    /// running or will not come back after a reboot.
    pub(super) daemon_problems: Vec<String>,
    /// `<host class>: <hours> behind <tag>` for each host that has lagged a
    /// published release for longer than the policy's `lag_alert`.
    pub(super) lagging_too_long: Vec<String>,
    pub(super) exit_code: u8,
}

pub(super) fn alert_title(tag: &str) -> String {
    format!("fleet-reconcile: {tag} could not reach the fleet")
}

/// Put the probed hosts in the report with what they show: which could not be
/// read, which are quarantined, and which declare themselves controllers.
fn record_hosts(report: &mut ReconcileReport, hosts: Vec<HostVersion>, ledger: &AttemptLedger) {
    report.unreachable = hosts
        .iter()
        .filter(|host| host.lagging.is_none())
        .map(|host| host.host_class.clone())
        .collect();
    report.quarantined = ledger.quarantined.keys().cloned().collect();
    report.other_controllers = hosts
        .iter()
        .filter(|host| host.declares_host_classes == Some(true))
        .map(|host| host.host_class.clone())
        .collect();
    report.hosts = hosts;
}

/// The recent releases and the newest of them by version, or why none could
/// be read.
fn read_releases<E: ReconcileEnv>(
    env: &mut E,
) -> (Result<PublishedRelease, String>, Vec<PublishedRelease>) {
    let releases = env.recent_releases();
    let latest = releases
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|releases| {
            releases
                .iter()
                .filter_map(|release| parse_version(&release.tag).map(|version| (version, release)))
                .max_by_key(|(version, _)| *version)
                .map(|(_, release)| release.clone())
                .ok_or_else(|| "no published vMAJOR.MINOR.PATCH release".to_owned())
        });
    (latest, releases.unwrap_or_default())
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
        unreachable: Vec::new(),
        quarantined: Vec::new(),
        other_controllers: Vec::new(),
        daemon_problems: Vec::new(),
        lagging_too_long: Vec::new(),
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
    let (latest, releases) = read_releases(env);
    let mut hosts = env.probe_hosts();
    if let Ok(latest) = &latest {
        classify_hosts(latest, &mut hosts);
    }
    report.latest_release = latest.as_ref().ok().cloned();
    let decision = match read_ledger(state_dir) {
        Ok(ledger) => decide(
            latest.as_ref().map_err(String::as_str),
            &releases,
            &hosts,
            &ledger,
            now,
            policy,
        ),
        Err(reason) => ReconcileDecision::Unknown { reason },
    };
    let ledger = read_ledger(state_dir).unwrap_or_default();
    record_hosts(&mut report, hosts, &ledger);
    report.decision = decision.clone();
    track_unreachable(env, &mut report, state_dir, latest.is_ok());
    track_daemon_problems(env, &mut report, state_dir);
    track_lag(
        env,
        &mut report,
        state_dir,
        &releases,
        now,
        policy.lag_alert,
    );
    let tag = report
        .latest_release
        .as_ref()
        .map_or_else(|| "?".to_owned(), |latest| latest.tag.clone());
    let mut exit = match &decision {
        ReconcileDecision::UpToDate | ReconcileDecision::Soaking { .. } => 0,
        ReconcileDecision::Unknown { .. } => EXIT_RECONCILE_UNKNOWN,
        ReconcileDecision::RateLimited { .. } => EXIT_RECONCILE_RATE_LIMITED,
        ReconcileDecision::Terminal { .. } => EXIT_RECONCILE_TERMINAL,
        ReconcileDecision::ControllerBusy => super::EXIT_CONTROLLER_BUSY,
        ReconcileDecision::Ahead { hosts } => {
            // Alert once per tag, not on every tick.
            if !ledger.ahead_alerted.contains(&tag) {
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
                let _ = update_ledger(state_dir, |ledger| {
                    ledger.ahead_alerted.insert(tag.clone());
                });
            }
            EXIT_RECONCILE_AHEAD
        }
        ReconcileDecision::Rollout { .. } if !apply => 0,
        ReconcileDecision::Rollout { tag, lagging } => {
            rollout(env, &mut report, state_dir, policy, tag, lagging, now)
        }
    };
    // A tick that could not read every host is not a clean pass.
    if exit == 0 && !report.unreachable.is_empty() {
        exit = EXIT_RECONCILE_UNKNOWN;
    }
    report.exit_code = exit;
    report
}

/// Count consecutive unreadable ticks per host and alert once a host has been
/// unreadable for [`UNREACHABLE_ALERT_TICKS`]. A host that answers again
/// resets its count and may alert again later.
fn track_unreachable<E: ReconcileEnv>(
    env: &mut E,
    report: &mut ReconcileReport,
    state_dir: &Path,
    release_read: bool,
) {
    if !release_read {
        return;
    }
    let unreachable = report.unreachable.clone();
    let reachable = report
        .hosts
        .iter()
        .filter(|host| host.lagging.is_some())
        .map(|host| host.host_class.clone())
        .collect::<Vec<_>>();
    let mut to_alert = Vec::new();
    let _ = update_ledger(state_dir, |ledger| {
        for class in &reachable {
            ledger.unreachable_ticks.remove(class);
            ledger.unreachable_alerted.remove(class);
        }
        for class in &unreachable {
            let ticks = ledger.unreachable_ticks.entry(class.clone()).or_insert(0);
            *ticks += 1;
            if *ticks >= UNREACHABLE_ALERT_TICKS && ledger.unreachable_alerted.insert(class.clone())
            {
                to_alert.push((class.clone(), *ticks));
            }
        }
    });
    for (class, ticks) in to_alert {
        let error = report
            .hosts
            .iter()
            .find(|host| host.host_class == class)
            .and_then(|host| host.error.clone())
            .unwrap_or_default();
        raise(
            env,
            report,
            &format!("fleet-reconcile: {class} is unreachable"),
            &format!(
                "fleet-reconcile could not read {class}'s installed Shipyard for {ticks} \
                 consecutive ticks ({error}). It is left out of every rollout until it answers."
            ),
        );
    }
}

/// Record each host class whose daemon is not running or will not come back
/// after a reboot in `daemon_problems`, and alert once per such class. The
/// alert is forgotten once the host reads healthy, so a recurrence alerts
/// again. Hosts whose daemon could not be read are left as they were.
fn track_daemon_problems<E: ReconcileEnv>(
    env: &mut E,
    report: &mut ReconcileReport,
    state_dir: &Path,
) {
    let healthy = report
        .hosts
        .iter()
        .filter(|host| {
            host.daemon
                .as_ref()
                .is_some_and(|daemon| daemon.problem().is_none())
        })
        .map(|host| host.host_class.clone())
        .collect::<Vec<_>>();
    let failing = report
        .hosts
        .iter()
        .filter_map(|host| Some((host.host_class.clone(), host.daemon.as_ref()?.problem()?)))
        .collect::<Vec<_>>();
    report.daemon_problems = failing
        .iter()
        .map(|(class, problem)| format!("{class}: {problem}"))
        .collect();
    let mut to_alert = Vec::new();
    let _ = update_ledger(state_dir, |ledger| {
        for class in &healthy {
            ledger.daemon_alerted.remove(class);
        }
        for (class, problem) in &failing {
            if ledger.daemon_alerted.insert(class.clone()) {
                to_alert.push((class.clone(), problem.clone()));
            }
        }
    });
    for (class, problem) in to_alert {
        raise(
            env,
            report,
            &format!("fleet-reconcile: {class} daemon will not survive a reboot"),
            &format!(
                "fleet-reconcile read {class}'s Shipyard daemon: {problem}. Until it is fixed the \
                 host can stop taking jobs after a reboot with nothing to say so."
            ),
        );
    }
}

/// Record each host that has lagged a published release for longer than
/// `threshold` in `lagging_too_long` and alert once per such class; a host
/// that catches up clears its alert. The lag runs from the earliest known
/// release newer than the host's version, so a soak that keeps restarting
/// cannot hide it. Unreadable hosts are left as they were.
fn track_lag<E: ReconcileEnv>(
    env: &mut E,
    report: &mut ReconcileReport,
    state_dir: &Path,
    releases: &[PublishedRelease],
    now: DateTime<Utc>,
    threshold: chrono::Duration,
) {
    let caught_up = report
        .hosts
        .iter()
        .filter(|host| host.lagging == Some(false))
        .map(|host| host.host_class.clone())
        .collect::<Vec<_>>();
    let mut overdue = Vec::new();
    for host in report
        .hosts
        .iter()
        .filter(|host| host.lagging == Some(true))
    {
        let Some(installed) = host.version.as_deref().and_then(parse_version) else {
            continue;
        };
        let newer = releases.iter().filter(|release| {
            parse_version(&release.tag).is_some_and(|version| version > installed)
        });
        let Some(since) = newer.clone().map(|release| release.published_at).min() else {
            continue;
        };
        let latest_tag = newer
            .filter_map(|release| parse_version(&release.tag).map(|version| (version, release)))
            .max_by_key(|(version, _)| *version)
            .map_or("?", |(_, release)| release.tag.as_str());
        let lag = now - since;
        if lag > threshold {
            overdue.push((
                host.host_class.clone(),
                lag.num_hours(),
                latest_tag.to_owned(),
            ));
        }
    }
    report.lagging_too_long = overdue
        .iter()
        .map(|(class, hours, tag)| format!("{class}: over {hours} h behind {tag}"))
        .collect();
    let mut to_alert = Vec::new();
    let _ = update_ledger(state_dir, |ledger| {
        for class in &caught_up {
            ledger.lag_alerted.remove(class);
        }
        for (class, hours, tag) in &overdue {
            if ledger.lag_alerted.insert(class.clone()) {
                to_alert.push((class.clone(), *hours, tag.clone()));
            }
        }
    });
    for (class, hours, tag) in to_alert {
        raise(
            env,
            report,
            &format!("fleet-reconcile: {class} has lagged the latest release for over {hours} h"),
            &format!(
                "{class} has not received a published Shipyard release for over {hours} h \
                 (newest available: {tag}). fleet-reconcile is still soaking or retrying; check \
                 its decision, or roll a soaked tag with `shipyard runner fleet-update`."
            ),
        );
    }
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
    // A canary (or an install) owning this host defers the tick before
    // anything is recorded: the ledger sits in the state tree that canary
    // audits, and a deferred tick is not an attempt.
    match env.local_host_busy() {
        Ok(None) => {}
        Ok(Some(reason)) => {
            report.rollout = Some(RolloutOutcome::Deferred { reason });
            return super::EXIT_CONTROLLER_BUSY;
        }
        Err(reason) => {
            report.decision = ReconcileDecision::Unknown { reason };
            return EXIT_RECONCILE_UNKNOWN;
        }
    }
    // Record before mutating: a crash or a failing rollout still counts as an
    // attempt, so a broken release cannot loop every tick.
    let prior = match read_ledger(state_dir) {
        Ok(ledger) => ledger.tags.get(tag).cloned(),
        Err(reason) => {
            report.decision = ReconcileDecision::Unknown { reason };
            return EXIT_RECONCILE_UNKNOWN;
        }
    };
    let attempt = match record_attempt(state_dir, tag, now) {
        Ok(attempt) => attempt,
        // A Sandbox audit held the writer domain through the bounded wait:
        // nothing was written and nothing rolled, so this is a deferral.
        Err(reason) if crate::writer_domain_lease::is_writer_domain_overlap(&reason) => {
            report.rollout = Some(RolloutOutcome::Deferred { reason });
            return super::EXIT_CONTROLLER_BUSY;
        }
        Err(reason) => {
            report.decision = ReconcileDecision::Unknown { reason };
            return EXIT_RECONCILE_UNKNOWN;
        }
    };
    report.attempt = Some(attempt);
    // Rolling out this tag means no older one will be retried: close those
    // here, where the ledger is being written anyway, so an idle tick still
    // writes nothing.
    if let Err(error) = retire_superseded(state_dir, tag) {
        report
            .alerts
            .push(format!("could not close superseded tags: {error}"));
    }
    let outcome = env.rollout(tag, lagging);
    report.rollout = Some(outcome.clone());
    let recorded = match &outcome {
        RolloutOutcome::Verified => Some("verified".to_owned()),
        RolloutOutcome::Failed { reason } => Some(format!("failed: {reason}")),
        _ => None,
    };
    if let Some(recorded) = recorded
        && let Err(error) = record_outcome(state_dir, tag, &recorded)
    {
        report
            .alerts
            .push(format!("could not record the rollout outcome: {error}"));
    }
    let terminal = match &outcome {
        RolloutOutcome::Verified => return 0,
        RolloutOutcome::Deferred { .. } => {
            // A busy host changed nothing, so this tick was not an attempt.
            // Withdraw exactly the record written above (the controller lock
            // is held, so nothing else wrote it) and let the next tick retry.
            if let Err(error) = withdraw_attempt(state_dir, tag, attempt, now, prior) {
                report
                    .alerts
                    .push(format!("could not withdraw the deferred attempt: {error}"));
            } else {
                report.attempt = None;
            }
            return super::EXIT_CONTROLLER_BUSY;
        }
        RolloutOutcome::RollbackFailed { host_class, reason } => {
            // Terminal at once and alerted at once: retrying would reinstall
            // onto a host nobody has looked at. The host stays out of every
            // future rollout until an operator clears it.
            let (terminal, body) =
                match record_rollback_failure(state_dir, tag, host_class, reason, now) {
                    Ok(recorded) => recorded,
                    Err(error) => {
                        report
                            .alerts
                            .push(format!("could not record the quarantine: {error}"));
                        (
                            format!("{}{reason}", rollback_failed_prefix(host_class)),
                            rollback_failure_body(tag, host_class, reason),
                        )
                    }
                };
            report.quarantined.push(host_class.clone());
            raise(env, report, &alert_title(tag), &body);
            report.terminal = Some(terminal);
            return super::EXIT_ROLLBACK_FAILED;
        }
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

const PROBE_CONTROLLER_MARKER: &str = "SHIPYARD_PROBE_DECLARES_HOST_CLASSES=";
const PROBE_LAUNCHER_MARKER: &str = "SHIPYARD_PROBE_DAEMON_LAUNCHER=";
const PROBE_DAEMON_MARKER: &str = "SHIPYARD_PROBE_DAEMON_STATUS=";

/// Read-only probe lines for the host's launcher and daemon, each one JSON
/// document on a line after its marker. Their exit codes never fail the
/// version probe.
fn daemon_probe_script(binary: &str) -> String {
    let binary = shlex_quote(binary);
    let launcher = shlex_quote(PROBE_LAUNCHER_MARKER);
    let daemon = shlex_quote(PROBE_DAEMON_MARKER);
    format!(
        "\nprintf '%s' {launcher}; {binary} --json daemon launcher status 2>/dev/null | /usr/bin/tr -d '\\n'; echo\nprintf '%s' {daemon}; {binary} --json daemon status 2>/dev/null | /usr/bin/tr -d '\\n'; echo"
    )
}

/// The daemon state the probe printed, when it printed any marker.
fn parse_daemon_probe(text: &str) -> Option<HostDaemon> {
    let field = |marker: &str, key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(marker))
            .and_then(|json| serde_json::from_str::<Value>(json).ok())
            .and_then(|value| value.get(key).and_then(Value::as_bool))
    };
    let daemon = HostDaemon {
        launcher_installed: field(PROBE_LAUNCHER_MARKER, "installed"),
        launcher_active: field(PROBE_LAUNCHER_MARKER, "active"),
        survives_kill_and_reboot: field(PROBE_LAUNCHER_MARKER, "survives_kill_and_reboot"),
        running: field(PROBE_DAEMON_MARKER, "running"),
    };
    (daemon != HostDaemon::default()).then_some(daemon)
}

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
        daemon: None,
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
    script.push_str(&daemon_probe_script(binary));
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
            result.daemon = parse_daemon_probe(&text);
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

/// Parse `GET repos/<repo>/releases`: the published, non-draft,
/// non-prerelease releases with a `vMAJOR.MINOR.PATCH` tag. Anything else is
/// skipped, never rolled.
pub(super) fn parse_recent_releases(value: &Value) -> Result<Vec<PublishedRelease>, String> {
    let releases = value
        .as_array()
        .ok_or_else(|| "the releases listing is not an array".to_owned())?
        .iter()
        .filter_map(|release| parse_latest_release(release).ok())
        .filter(|release| parse_version(&release.tag).is_some())
        .collect::<Vec<_>>();
    if releases.is_empty() {
        return Err("no published vMAJOR.MINOR.PATCH release in the listing".to_owned());
    }
    Ok(releases)
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
            // Long enough that only the lag tests, which set their own, see
            // a lag alert among the alerts these tests count.
            lag_alert: chrono::Duration::days(365),
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
            declares_host_classes: None,
            daemon: None,
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
            std::slice::from_ref(latest),
            &hosts,
            ledger,
            now,
            policy(),
        )
    }

    #[test]
    fn a_release_stream_rolls_the_newest_soaked_tag_never_an_unsoaked_one() {
        // A release every 10 minutes for an hour, soaked for 30 minutes each.
        let t0 = Utc::now();
        let stream = (0..=6)
            .map(|n| PublishedRelease {
                tag: format!("v1.0.{n}"),
                published_at: t0 + chrono::Duration::minutes(10 * n),
            })
            .collect::<Vec<_>>();
        let mut rolled = Vec::new();
        for tick in 0..=20 {
            let now = t0 + chrono::Duration::minutes(5 * tick);
            let published = stream
                .iter()
                .filter(|release| release.published_at <= now)
                .cloned()
                .collect::<Vec<_>>();
            let latest = published.last().expect("v1.0.0 is published at t0").clone();
            let mut hosts = vec![host("m1", Some("0.9.0"))];
            classify_hosts(&latest, &mut hosts);
            let decision = decide(
                Ok(&latest),
                &published,
                &hosts,
                &AttemptLedger::default(),
                now,
                policy(),
            );
            let soaked = published
                .iter()
                .rfind(|release| release.published_at + policy().soak <= now);
            match (soaked, &decision) {
                (None, ReconcileDecision::Soaking { soak_until, .. }) => {
                    assert_eq!(*soak_until, t0 + policy().soak, "tick {tick}");
                }
                (Some(expected), ReconcileDecision::Rollout { tag, lagging }) => {
                    assert_eq!(tag, &expected.tag, "tick {tick}: the newest soaked tag");
                    assert_eq!(lagging, &["m1"]);
                    rolled.push(tag.clone());
                }
                other => panic!("tick {tick}: {other:?}"),
            }
        }
        // Control: the stream did outrun the soak. At every rolling tick a
        // newer, unsoaked tag existed until publishing stopped, yet rollout
        // never waited for it.
        assert_eq!(rolled.first().map(String::as_str), Some("v1.0.0"));
        assert_eq!(rolled.last().map(String::as_str), Some("v1.0.6"));
        assert_eq!(rolled.len(), 15, "{rolled:?}");
    }

    #[test]
    fn hosts_on_the_newest_soaked_tag_wait_for_the_next_to_soak() {
        let now = Utc::now();
        let releases = vec![release("v1.0.0", 60, now), release("v1.0.1", 10, now)];
        let latest = releases[1].clone();
        let mut hosts = vec![host("m1", Some("1.0.0"))];
        classify_hosts(&latest, &mut hosts);
        let decision = decide(
            Ok(&latest),
            &releases,
            &hosts,
            &AttemptLedger::default(),
            now,
            policy(),
        );
        assert_eq!(
            decision,
            ReconcileDecision::Soaking {
                soak_until: releases[1].published_at + policy().soak,
                lagging: vec!["m1".to_owned()],
            }
        );
    }

    #[test]
    fn the_releases_listing_keeps_only_published_semver_tags() {
        let release = |tag: &str, draft: bool, prerelease: bool| {
            serde_json::json!({"tag_name": tag, "draft": draft, "prerelease": prerelease,
                "published_at": "2026-10-06T03:59:22Z"})
        };
        let listing = serde_json::json!([
            release("v0.278.1", false, false),
            release("v0.279.0", true, false),
            release("v0.280.0-rc1", false, true),
            release("nightly", false, false),
            release("v0.277.0", false, false),
        ]);
        let tags = parse_recent_releases(&listing)
            .expect("two releases")
            .into_iter()
            .map(|release| release.tag)
            .collect::<Vec<_>>();
        assert_eq!(tags, ["v0.278.1", "v0.277.0"]);
        assert!(
            parse_recent_releases(&serde_json::json!([release("nightly", false, false)])).is_err()
        );
        assert!(parse_recent_releases(&serde_json::json!({})).is_err());
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
                tag: "v0.208.0".to_owned(),
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
    fn unreadable_inputs_are_never_read_as_current() {
        let now = Utc::now();
        let latest = release("v0.208.0", 120, now);
        // One unreadable host is left out; the readable ones still decide.
        let decision = run(
            &latest,
            vec![host("m1", Some("0.205.0")), host("m5", None)],
            &AttemptLedger::default(),
            now,
        );
        assert_eq!(
            decision,
            ReconcileDecision::Rollout {
                tag: latest.tag.clone(),
                lagging: vec!["m1".to_owned()]
            }
        );
        // Every host unreadable is unknown.
        let decision = run(
            &latest,
            vec![host("m1", None), host("m5", None)],
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
        busy: Option<String>,
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
                busy: None,
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
        fn local_host_busy(&mut self) -> Result<Option<String>, String> {
            Ok(self.busy.clone())
        }
    }

    fn failed() -> RolloutOutcome {
        RolloutOutcome::Failed {
            reason: "m5 failed post-rollout verification".to_owned(),
        }
    }

    #[test]
    fn a_busy_controller_host_defers_before_writing_the_ledger() {
        let temp = tempfile::tempdir().expect("temp");
        let mut env = FakeEnv::new(
            Utc::now(),
            &[("m1", "0.208.0"), ("m5", "0.205.0")],
            RolloutOutcome::Verified,
        );
        env.busy = Some("sandbox canary owns this host".to_owned());
        let report = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(report.exit_code, super::super::EXIT_CONTROLLER_BUSY);
        assert!(env.rollouts.is_empty(), "a busy host is never rolled");
        assert_eq!(report.attempt, None);
        assert!(matches!(
            report.rollout,
            Some(RolloutOutcome::Deferred { .. })
        ));
        assert!(
            !ledger_path(temp.path()).exists(),
            "a deferred tick writes nothing into the audited state tree"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_idle_tick_leaves_the_ledger_file_untouched() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().expect("temp");
        record_attempt(temp.path(), "v0.207.0", Utc::now()).expect("seed");
        let identity = || {
            let metadata = std::fs::metadata(ledger_path(temp.path())).expect("ledger");
            (metadata.ino(), metadata.mtime_nsec(), metadata.mtime())
        };
        let before = identity();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // An up-to-date tick with every host reachable changes nothing.
        let mut env = FakeEnv::new(
            Utc::now(),
            &[("m1", "0.208.0"), ("m5", "0.208.0")],
            RolloutOutcome::Verified,
        );
        let report = run_reconcile(&mut env, temp.path(), policy(), true);
        assert!(matches!(report.decision, ReconcileDecision::UpToDate));
        assert_eq!(identity(), before, "an idle tick rewrote the ledger");
        // A real change still persists.
        update_ledger(temp.path(), |ledger| {
            ledger.ahead_alerted.insert("v0.208.0".to_owned());
        })
        .expect("update");
        assert_ne!(identity(), before);
    }

    #[test]
    fn install_guard_probe_sees_another_holder_and_never_creates_the_guard() {
        let temp = tempfile::tempdir().expect("temp");
        let guard = temp
            .path()
            .join(super::super::auth_support::INSTALL_GUARD_NAME);
        assert_eq!(local_install_guard_held(temp.path()), Ok(None));
        assert!(!guard.exists(), "probing must not create the guard");
        std::fs::write(&guard, b"").expect("guard");
        assert_eq!(local_install_guard_held(temp.path()), Ok(None));
        let holder = std::fs::File::open(&guard).expect("holder");
        fs2::FileExt::lock_exclusive(&holder).expect("hold");
        assert!(
            local_install_guard_held(temp.path())
                .expect("probe")
                .is_some_and(|reason| reason.contains("recorded nothing"))
        );
        fs2::FileExt::unlock(&holder).expect("release");
        assert_eq!(local_install_guard_held(temp.path()), Ok(None));
    }

    #[test]
    fn a_busy_host_defers_without_spending_an_attempt() {
        let temp = tempfile::tempdir().expect("temp");
        let start = Utc::now();
        let mut env = FakeEnv::new(
            start,
            &[("m1", "0.208.0"), ("m5", "0.205.0")],
            RolloutOutcome::Deferred {
                reason: "m5 is busy".to_owned(),
            },
        );
        let deferred = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(deferred.exit_code, super::super::EXIT_CONTROLLER_BUSY);
        assert_eq!(deferred.attempt, None);
        assert!(deferred.terminal.is_none() && env.alerts.is_empty());
        assert!(
            !read_ledger(temp.path())
                .expect("ledger")
                .tags
                .contains_key("v0.208.0")
        );

        // The next tick is not rate-limited by the deferral: it rolls again,
        // and a real failure then counts as the first attempt.
        env.outcome = failed();
        let next = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(env.rollouts.len(), 2);
        assert_eq!(next.attempt, Some(1));

        // A deferral after a real attempt restores that attempt exactly.
        let recorded = read_ledger(temp.path()).expect("ledger").tags["v0.208.0"].clone();
        env.now = start + chrono::Duration::hours(7);
        env.outcome = RolloutOutcome::Deferred {
            reason: "m5 is busy".to_owned(),
        };
        run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(
            read_ledger(temp.path()).expect("ledger").tags["v0.208.0"],
            recorded
        );
    }

    #[test]
    fn a_failed_tag_becomes_terminal_once_a_newer_release_supersedes_it() {
        let temp = tempfile::tempdir().expect("temp");
        let start = Utc::now();
        let mut env = FakeEnv::new(start, &[("m1", "0.208.0"), ("m5", "0.205.0")], failed());
        let first = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(first.exit_code, 1);
        let entry = read_ledger(temp.path()).expect("ledger").tags["v0.208.0"].clone();
        assert!(
            entry
                .last_outcome
                .as_deref()
                .is_some_and(|o| o.starts_with("failed: ")),
            "{entry:?}"
        );
        assert!(
            entry.terminal.is_none(),
            "still retryable while it is the latest"
        );

        // A newer release lands and rolls out cleanly: the failed older tag is
        // never retried again, so it must stop reading as open.
        env.latest = Ok(release("v0.209.0", 120, start));
        env.outcome = RolloutOutcome::Verified;
        env.now = start + chrono::Duration::hours(1);
        let next = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(next.exit_code, 0, "{:?}", next.decision);
        let ledger = read_ledger(temp.path()).expect("ledger");
        let old = &ledger.tags["v0.208.0"];
        assert!(
            old.terminal
                .as_deref()
                .is_some_and(|reason| reason.starts_with("superseded by v0.209.0")
                    && reason.contains("failed: ")),
            "{old:?}"
        );
        let new = &ledger.tags["v0.209.0"];
        assert_eq!(new.last_outcome.as_deref(), Some("verified"));
        assert!(new.terminal.is_none());
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

    #[test]
    fn a_failed_rollback_alerts_at_once_and_quarantines_the_host() {
        let temp = tempfile::tempdir().expect("temp");
        let start = Utc::now();
        let mut env = FakeEnv::new(
            start,
            &[("m1", "0.205.0"), ("m5", "0.205.0")],
            RolloutOutcome::RollbackFailed {
                host_class: "m5".to_owned(),
                reason: "ROLLBACK TO v0.205.0 FAILED: daemon is not running".to_owned(),
            },
        );
        let report = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(report.exit_code, super::super::EXIT_ROLLBACK_FAILED);
        assert_eq!(report.attempt, Some(1), "no waiting for the attempt cap");
        assert_eq!(env.alerts.len(), 1, "alerted on the first failure");
        assert!(env.alerts[0].1.contains("m5 needs an operator"));
        assert!(env.alerts[0].1.contains("--clear-host m5"));
        let ledger = read_ledger(temp.path()).expect("ledger");
        assert!(ledger.quarantined.contains_key("m5"));
        assert!(ledger.tags["v0.208.0"].terminal.is_some());

        // A newer release must still never touch the quarantined host.
        env.latest = Ok(release("v0.209.0", 120, start));
        env.outcome = RolloutOutcome::Verified;
        env.now = start + chrono::Duration::days(1);
        let later = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(later.quarantined, ["m5"]);
        assert_eq!(env.rollouts.last().expect("rollout").1, ["m1"]);
        assert_eq!(env.alerts.len(), 1);

        // Once an operator clears it, the host and its terminal tag return.
        let cleared = clear_host(temp.path(), "m5")
            .expect("clear")
            .expect("was quarantined");
        assert_eq!(cleared.tag, "v0.208.0");
        let ledger = read_ledger(temp.path()).expect("ledger");
        assert!(ledger.quarantined.is_empty());
        assert!(ledger.tags["v0.208.0"].terminal.is_none());
        env.hosts[0].1 = Some("0.209.0".to_owned());
        env.now = start + chrono::Duration::days(2);
        run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(env.rollouts.last().expect("rollout").1, ["m5"]);
    }

    #[test]
    fn a_host_ahead_is_alerted_once_per_tag() {
        let temp = tempfile::tempdir().expect("temp");
        let start = Utc::now();
        let mut env = FakeEnv::new(start, &[("m5", "0.210.0")], RolloutOutcome::Verified);
        for tick in 0..4 {
            env.now = start + chrono::Duration::minutes(15 * tick);
            let report = run_reconcile(&mut env, temp.path(), policy(), true);
            assert_eq!(report.exit_code, EXIT_RECONCILE_AHEAD);
        }
        assert_eq!(env.alerts.len(), 1, "one alert per tag, not one per tick");
        env.latest = Ok(release("v0.209.0", 120, start));
        run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(env.alerts.len(), 2, "a different latest tag alerts again");
    }

    #[test]
    fn an_unreachable_host_does_not_block_the_others_and_alerts_after_a_streak() {
        let temp = tempfile::tempdir().expect("temp");
        let start = Utc::now();
        let mut env = FakeEnv::new(
            start,
            &[("m1", "0.205.0"), ("m5", "0.205.0")],
            RolloutOutcome::Verified,
        );
        env.hosts[1].1 = None;
        let first = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(
            env.rollouts,
            vec![("v0.208.0".to_owned(), vec!["m1".to_owned()])]
        );
        assert_eq!(first.unreachable, ["m5"]);
        assert_eq!(
            first.exit_code, EXIT_RECONCILE_UNKNOWN,
            "a partial tick is not clean"
        );
        env.hosts[0].1 = Some("0.208.0".to_owned());
        for tick in 1..UNREACHABLE_ALERT_TICKS + 3 {
            env.now = start + chrono::Duration::minutes(15 * i64::from(tick));
            run_reconcile(&mut env, temp.path(), policy(), true);
        }
        let unreachable_alerts = env
            .alerts
            .iter()
            .filter(|(title, _)| title == "fleet-reconcile: m5 is unreachable")
            .count();
        assert_eq!(
            unreachable_alerts, 1,
            "alert once after the streak, not every tick"
        );
        // Answering again resets the streak.
        env.hosts[1].1 = Some("0.208.0".to_owned());
        run_reconcile(&mut env, temp.path(), policy(), true);
        assert!(
            read_ledger(temp.path())
                .expect("ledger")
                .unreachable_ticks
                .is_empty()
        );
    }

    #[test]
    fn a_second_controller_is_reported() {
        struct Controllers(FakeEnv);
        impl ReconcileEnv for Controllers {
            fn now(&self) -> DateTime<Utc> {
                self.0.now()
            }
            fn latest_release(&mut self) -> Result<PublishedRelease, String> {
                self.0.latest_release()
            }
            fn probe_hosts(&mut self) -> Vec<HostVersion> {
                let mut hosts = self.0.probe_hosts();
                hosts[0].declares_host_classes = Some(true);
                hosts
            }
            fn rollout(&mut self, tag: &str, host_classes: &[String]) -> RolloutOutcome {
                self.0.rollout(tag, host_classes)
            }
            fn alert(&mut self, title: &str, body: &str) -> Result<(), String> {
                self.0.alert(title, body)
            }
        }
        let temp = tempfile::tempdir().expect("temp");
        let mut env = Controllers(FakeEnv::new(
            Utc::now(),
            &[("m5", "0.208.0")],
            RolloutOutcome::Verified,
        ));
        let report = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(report.other_controllers, ["m5"]);
    }

    #[test]
    fn a_host_lagging_past_the_threshold_alerts_once_until_it_catches_up() {
        struct Stream(FakeEnv, Vec<PublishedRelease>);
        impl ReconcileEnv for Stream {
            fn now(&self) -> DateTime<Utc> {
                self.0.now()
            }
            fn latest_release(&mut self) -> Result<PublishedRelease, String> {
                self.0.latest_release()
            }
            fn recent_releases(&mut self) -> Result<Vec<PublishedRelease>, String> {
                Ok(self.1.clone())
            }
            fn probe_hosts(&mut self) -> Vec<HostVersion> {
                self.0.probe_hosts()
            }
            fn rollout(&mut self, tag: &str, host_classes: &[String]) -> RolloutOutcome {
                self.0.rollout(tag, host_classes)
            }
            fn alert(&mut self, title: &str, body: &str) -> Result<(), String> {
                self.0.alert(title, body)
            }
        }
        let temp = tempfile::tempdir().expect("temp");
        let now = Utc::now();
        let lag_policy = ReconcilePolicy {
            lag_alert: chrono::Duration::hours(2),
            ..policy()
        };
        // v0.209.0 has been out three hours; every newer release keeps
        // restarting the soak, so m5 never received any of them.
        let releases = vec![release("v0.209.0", 180, now), release("v0.210.0", 5, now)];
        let mut env = Stream(
            FakeEnv::new(
                now,
                &[("m1", "0.210.0"), ("m5", "0.208.0")],
                RolloutOutcome::Verified,
            ),
            releases,
        );
        let report = run_reconcile(&mut env, temp.path(), lag_policy, false);
        assert_eq!(report.lagging_too_long, ["m5: over 3 h behind v0.210.0"]);
        assert_eq!(env.0.alerts.len(), 1, "{:?}", env.0.alerts);
        assert!(env.0.alerts[0].0.contains("m5 has lagged"));

        run_reconcile(&mut env, temp.path(), lag_policy, false);
        assert_eq!(
            env.0.alerts.len(),
            1,
            "one alert per host until it catches up"
        );

        env.0.hosts = vec![
            ("m1".to_owned(), Some("0.210.0".to_owned())),
            ("m5".to_owned(), Some("0.210.0".to_owned())),
        ];
        assert!(
            run_reconcile(&mut env, temp.path(), lag_policy, false)
                .lagging_too_long
                .is_empty()
        );
        env.0.hosts[1].1 = Some("0.208.0".to_owned());
        run_reconcile(&mut env, temp.path(), lag_policy, false);
        assert_eq!(
            env.0.alerts.len(),
            2,
            "lagging again after catching up alerts again"
        );

        // Control: under the threshold nothing is reported.
        let quiet = tempfile::tempdir().expect("temp");
        let mut recent = Stream(
            FakeEnv::new(now, &[("m5", "0.208.0")], RolloutOutcome::Verified),
            vec![release("v0.209.0", 60, now)],
        );
        assert!(
            run_reconcile(&mut recent, quiet.path(), lag_policy, false)
                .lagging_too_long
                .is_empty()
        );
    }

    fn daemon(installed: bool, active: bool, running: bool) -> HostDaemon {
        HostDaemon {
            launcher_installed: Some(installed),
            launcher_active: Some(active),
            survives_kill_and_reboot: (installed && active).then_some(true),
            running: Some(running),
        }
    }

    #[test]
    fn an_active_launcher_is_reboot_safe_only_when_its_plist_says_so() {
        let with = |survives| HostDaemon {
            survives_kill_and_reboot: survives,
            ..daemon(true, true, true)
        };
        assert_eq!(with(Some(true)).problem(), None);
        let off = with(Some(false))
            .problem()
            .expect("plist with KeepAlive off is a problem");
        assert!(off.contains("RunAtLoad or KeepAlive off"), "{off}");
        assert!(off.contains("`shipyard daemon refresh`"), "{off}");
        let unread = with(None).problem().expect("an unread plist is not a yes");
        assert!(unread.contains("could not be read"), "{unread}");
        let text = format!(
            "{PROBE_LAUNCHER_MARKER}{{\"active\": true, \"installed\": true, \"survives_kill_and_reboot\": false}}\n\
             {PROBE_DAEMON_MARKER}{{\"running\": true}}\n"
        );
        assert_eq!(parse_daemon_probe(&text), Some(with(Some(false))));
    }

    #[test]
    fn a_daemon_problem_names_the_step_that_fixes_it() {
        assert_eq!(daemon(true, true, true).problem(), None);
        assert_eq!(
            HostDaemon::default().problem(),
            None,
            "nothing read, nothing claimed"
        );
        let missing = daemon(false, false, true).problem().expect("problem");
        assert!(missing.contains("not installed"), "{missing}");
        assert!(
            missing.contains("`shipyard daemon launcher install`"),
            "{missing}"
        );
        assert!(missing.contains("console"), "{missing}");
        let inactive = daemon(true, false, true).problem().expect("problem");
        assert!(inactive.contains("inactive"), "{inactive}");
        let stopped = daemon(true, true, false).problem().expect("problem");
        assert!(stopped.contains("not running"), "{stopped}");
        assert!(stopped.contains("`shipyard daemon refresh`"), "{stopped}");
        let both = daemon(false, false, false).problem().expect("problem");
        assert!(
            both.contains("not running") && both.contains("not installed"),
            "{both}"
        );
    }

    #[test]
    fn the_probe_reads_the_launcher_and_daemon_status_lines() {
        let text = format!(
            "shipyard 0.276.0\n{PROBE_LAUNCHER_MARKER}{{\"active\": false, \"installed\": false, \"record\": null}}\n\
             {PROBE_DAEMON_MARKER}{{\"running\": true, \"shipyard_version\": \"0.276.0\"}}\n"
        );
        assert_eq!(parse_daemon_probe(&text), Some(daemon(false, false, true)));
        assert_eq!(
            parse_daemon_probe("shipyard 0.276.0\n"),
            None,
            "an older host prints no markers"
        );
        let garbled = format!("{PROBE_LAUNCHER_MARKER}not json\n{PROBE_DAEMON_MARKER}\n");
        assert_eq!(
            parse_daemon_probe(&garbled),
            None,
            "unreadable output claims nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_host_probe_runs_the_daemon_status_commands() {
        let temp = tempfile::tempdir().expect("temp");
        let binary = temp.path().join("shipyard");
        crate::test_support::write_executable_script(
            &binary,
            "#!/bin/sh\n\
             case \"$*\" in\n\
               --version) echo 'shipyard 0.276.0' ;;\n\
               '--json daemon launcher status') printf '{\\n  \"active\": false,\\n  \"installed\": false\\n}\\n' ;;\n\
               '--json daemon status') printf '{\\n  \"running\": false\\n}\\n' ;;\n\
               *) echo \"unexpected: $*\" >&2; exit 2 ;;\n\
             esac\n",
        );
        let host = probe_version_at("m5", None, binary.to_str(), None);
        assert_eq!(host.version.as_deref(), Some("0.276.0"), "{:?}", host.error);
        assert_eq!(host.daemon, Some(daemon(false, false, false)));
    }

    #[test]
    fn a_daemon_problem_alerts_once_until_the_host_reads_healthy() {
        struct Daemons(FakeEnv, HostDaemon);
        impl ReconcileEnv for Daemons {
            fn now(&self) -> DateTime<Utc> {
                self.0.now()
            }
            fn latest_release(&mut self) -> Result<PublishedRelease, String> {
                self.0.latest_release()
            }
            fn probe_hosts(&mut self) -> Vec<HostVersion> {
                let mut hosts = self.0.probe_hosts();
                hosts[0].daemon = Some(self.1.clone());
                hosts
            }
            fn rollout(&mut self, tag: &str, host_classes: &[String]) -> RolloutOutcome {
                self.0.rollout(tag, host_classes)
            }
            fn alert(&mut self, title: &str, body: &str) -> Result<(), String> {
                self.0.alert(title, body)
            }
        }
        let temp = tempfile::tempdir().expect("temp");
        let mut env = Daemons(
            FakeEnv::new(Utc::now(), &[("m5", "0.208.0")], RolloutOutcome::Verified),
            daemon(false, false, false),
        );
        let report = run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(report.daemon_problems.len(), 1);
        assert!(
            report.daemon_problems[0].starts_with("m5: "),
            "{:?}",
            report.daemon_problems
        );
        assert_eq!(
            report.exit_code, 0,
            "a daemon problem never blocks a rollout decision"
        );
        assert_eq!(env.0.alerts.len(), 1);
        assert!(
            env.0.alerts[0]
                .1
                .contains("shipyard daemon launcher install")
        );

        run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(env.0.alerts.len(), 1, "the same problem alerts once");

        env.1 = daemon(true, true, true);
        let healthy = run_reconcile(&mut env, temp.path(), policy(), true);
        assert!(healthy.daemon_problems.is_empty());
        env.1 = daemon(false, false, true);
        run_reconcile(&mut env, temp.path(), policy(), true);
        assert_eq!(
            env.0.alerts.len(),
            2,
            "a recurrence after a healthy read alerts again"
        );
    }
}
