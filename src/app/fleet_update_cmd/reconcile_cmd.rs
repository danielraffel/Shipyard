//! `shipyard runner fleet-reconcile` and `shipyard doctor --fleet`: the live
//! wiring around [`super::reconcile`].
//!
//! Every host is enumerated from the machine-global `[host_class.<name>]`
//! configuration. No host is named in code, so adding a machine is adding its
//! host class (and its tartci profile); it is then probed, reported and rolled
//! like every other host.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::reconcile::{
    self, HostVersion, PublishedRelease, ReconcileDecision, ReconcileEnv, ReconcilePolicy,
    ReconcileReport, RolloutOutcome,
};
use super::{FleetUpdateArgs, GitHubReleaseAuthorityVerifier, release_authority, run_fleet_update};
use crate::app::CliFailure;
use crate::capacity::{HostClassConfig, parse_host_classes};
use crate::config::LoadedConfig;
use crate::doctor::DoctorEntry;
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;

pub(in crate::app) struct FleetReconcileArgs {
    pub(in crate::app) soak_minutes: u64,
    pub(in crate::app) retry_hours: u64,
    pub(in crate::app) max_attempts: u32,
    pub(in crate::app) clear_host: Option<String>,
    pub(in crate::app) apply: bool,
}

struct LiveEnv<'a> {
    config: &'a LoadedConfig,
    classes: Vec<HostClassConfig>,
    mode: RuntimeMode,
    cwd: &'a Path,
    runtime_paths: &'a RuntimePaths,
    json: bool,
    rollout_output: Vec<u8>,
}

fn latest_release(config: &LoadedConfig, cwd: &Path) -> Result<PublishedRelease, String> {
    let repository = release_authority::release_repository()?;
    GitHubReleaseAuthorityVerifier::new(config, cwd)
        .api_json(&format!("repos/{repository}/releases/latest"))
        .and_then(|value| reconcile::parse_latest_release(&value))
}

impl ReconcileEnv for LiveEnv<'_> {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn latest_release(&mut self) -> Result<PublishedRelease, String> {
        latest_release(self.config, self.cwd)
    }

    fn probe_hosts(&mut self) -> Vec<HostVersion> {
        self.classes
            .iter()
            .map(reconcile::probe_host_version)
            .collect()
    }

    fn rollout(&mut self, tag: &str, host_classes: &[String]) -> RolloutOutcome {
        let args = FleetUpdateArgs {
            to: tag.to_owned(),
            host_classes: host_classes.to_vec(),
            all_hosts: false,
            apply: true,
            // Re-checked at apply time: a host that caught up since the
            // decision is skipped rather than reinstalled.
            lagging_only: true,
        };
        // The caller already holds the controller lock.
        match run_fleet_update(
            &args,
            self.mode,
            self.cwd,
            self.runtime_paths,
            self.json,
            &mut self.rollout_output,
        ) {
            Ok(_) => RolloutOutcome::Verified,
            Err(failure) if failure.rollback_failed_host.is_some() => {
                RolloutOutcome::RollbackFailed {
                    host_class: failure.rollback_failed_host.unwrap_or_default(),
                    reason: failure.failure.message().to_owned(),
                }
            }
            Err(failure) if failure.ineligible => RolloutOutcome::Ineligible {
                reason: failure.failure.message().to_owned(),
            },
            Err(failure) => RolloutOutcome::Failed {
                reason: failure.failure.message().to_owned(),
            },
        }
    }

    fn alert(&mut self, title: &str, body: &str) -> Result<(), String> {
        GitHubReleaseAuthorityVerifier::new(self.config, self.cwd).upsert_issue(title, body)
    }
}

/// `shipyard runner fleet-reconcile`: roll the latest published release out to
/// the host classes that lag it, once it has soaked. See [`reconcile`].
pub(in crate::app) fn fleet_reconcile_command<W: Write>(
    args: &FleetReconcileArgs,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if cfg!(not(unix)) {
        return Err(CliFailure::new(
            1,
            "fleet-reconcile requires a Unix rollout controller",
        ));
    }
    if let Some(host_class) = &args.clear_host {
        let Some(_lock) = super::controller_lock::try_acquire(&runtime_paths.state_dir)
            .map_err(|error| CliFailure::new(1, error))?
        else {
            return Err(CliFailure::new(
                super::EXIT_CONTROLLER_BUSY,
                "another fleet rollout holds the controller lock; retry --clear-host later",
            ));
        };
        let cleared = reconcile::clear_host(&runtime_paths.state_dir, host_class)
            .map_err(|error| CliFailure::new(1, error))?;
        let message = match cleared {
            Some(quarantine) => format!(
                "cleared {host_class} (quarantined since {} after {}: {})",
                quarantine.since, quarantine.tag, quarantine.reason
            ),
            None => format!("{host_class} was not quarantined"),
        };
        writeln!(stdout, "{message}").map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    }
    let config = LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let classes = parse_host_classes(&config.data).map_err(|error| CliFailure::new(2, error))?;
    let mut env = LiveEnv {
        config: &config,
        classes,
        mode,
        cwd,
        runtime_paths,
        json,
        rollout_output: Vec::new(),
    };
    let policy = ReconcilePolicy {
        soak: chrono::Duration::minutes(i64::try_from(args.soak_minutes).unwrap_or(i64::MAX / 120)),
        retry: chrono::Duration::hours(i64::try_from(args.retry_hours).unwrap_or(i64::MAX / 7200)),
        max_attempts: args.max_attempts.max(1),
    };
    let report = reconcile::run_reconcile(&mut env, &runtime_paths.state_dir, policy, args.apply);
    stdout
        .write_all(&env.rollout_output)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    render_reconcile(stdout, json, &report, args.apply)?;
    if report.exit_code == 0 {
        return Ok(ExitCode::SUCCESS);
    }
    Err(CliFailure::new(report.exit_code, failure_message(&report)))
}

fn failure_message(report: &ReconcileReport) -> String {
    if let Some(RolloutOutcome::RollbackFailed { host_class, reason }) = &report.rollout {
        return format!(
            "{host_class} could not be rolled back and needs an operator ({reason}); it is \
             quarantined until `shipyard runner fleet-reconcile --clear-host {host_class}`"
        );
    }
    if report.exit_code == reconcile::EXIT_RECONCILE_UNKNOWN && !report.unreachable.is_empty() {
        return format!(
            "could not read {}; reachable hosts were reconciled, these were left out",
            report.unreachable.join(", ")
        );
    }
    match (&report.decision, &report.rollout) {
        (_, Some(RolloutOutcome::Failed { reason })) if report.terminal.is_none() => format!(
            "fleet rollout attempt {} failed: {reason}",
            report.attempt.unwrap_or_default()
        ),
        (_, Some(_)) => format!(
            "fleet-reconcile stopped retrying: {}",
            report.terminal.as_deref().unwrap_or("unknown")
        ),
        (ReconcileDecision::Unknown { reason }, _) => format!(
            "fleet-reconcile could not determine fleet skew; nothing was rolled out: {reason}"
        ),
        (
            ReconcileDecision::RateLimited {
                next_attempt,
                lagging,
                ..
            },
            _,
        ) => format!(
            "hosts still lag ({}) and this release was attempted recently; next attempt after {next_attempt}",
            lagging.join(", ")
        ),
        (ReconcileDecision::Terminal { reason, lagging }, _) => format!(
            "hosts still lag ({}) and this release is terminal: {reason}",
            lagging.join(", ")
        ),
        (ReconcileDecision::Ahead { hosts }, _) => format!(
            "hosts run a newer version than the latest release ({}); nothing was rolled out",
            hosts.join(", ")
        ),
        (ReconcileDecision::ControllerBusy, _) => {
            "another fleet rollout holds the controller lock; this tick did nothing".to_owned()
        }
        _ => "fleet-reconcile did not complete".to_owned(),
    }
}

fn render_reconcile<W: Write>(
    stdout: &mut W,
    json: bool,
    report: &ReconcileReport,
    apply: bool,
) -> Result<(), CliFailure> {
    let fail = |error: String| CliFailure::new(1, error);
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("fleet_reconcile"));
        data.insert("apply".to_owned(), Value::Bool(apply));
        let Value::Object(fields) =
            serde_json::to_value(report).map_err(|error| fail(error.to_string()))?
        else {
            return Err(fail("reconcile report must be an object".to_owned()));
        };
        data.extend(fields);
        write_json_envelope(stdout, "runner.fleet-reconcile", data)
            .map_err(|error| fail(error.to_string()))?;
        return Ok(());
    }
    let io = |error: std::io::Error| fail(error.to_string());
    match &report.latest_release {
        Some(latest) => writeln!(
            stdout,
            "latest release {} (published {})",
            latest.tag, latest.published_at
        )
        .map_err(io)?,
        None => writeln!(stdout, "latest release UNKNOWN").map_err(io)?,
    }
    for host in &report.hosts {
        writeln!(
            stdout,
            "  {:<14} {}{}",
            host.host_class,
            host.version.as_deref().unwrap_or("UNKNOWN"),
            match (host.lagging, host.error.as_deref()) {
                (Some(true), _) => " LAGGING".to_owned(),
                (_, Some(error)) => format!(" ({error})"),
                _ => String::new(),
            }
        )
        .map_err(io)?;
    }
    writeln!(stdout, "decision: {}", describe(&report.decision, apply)).map_err(io)?;
    for class in &report.unreachable {
        writeln!(stdout, "UNREACHABLE: {class} (left out of this tick)").map_err(io)?;
    }
    for class in &report.quarantined {
        writeln!(
            stdout,
            "QUARANTINED: {class} (needs an operator; --clear-host {class} once fixed)"
        )
        .map_err(io)?;
    }
    for class in &report.other_controllers {
        writeln!(
            stdout,
            "WARNING: {class} also declares [host_class.*]; the fleet must have exactly one controller"
        )
        .map_err(io)?;
    }
    for alert in &report.alerts {
        writeln!(stdout, "ALERT: {alert}").map_err(io)?;
    }
    Ok(())
}

fn describe(decision: &ReconcileDecision, apply: bool) -> String {
    match decision {
        ReconcileDecision::UpToDate => "every host is current".to_owned(),
        ReconcileDecision::Soaking {
            soak_until,
            lagging,
        } => format!(
            "{} lag; release soaks until {soak_until}",
            lagging.join(", ")
        ),
        ReconcileDecision::RateLimited {
            next_attempt,
            lagging,
            ..
        } => format!(
            "{} lag; already attempted, next attempt after {next_attempt}",
            lagging.join(", ")
        ),
        ReconcileDecision::Terminal { reason, lagging } => format!(
            "{} lag; TERMINAL, not retried: {reason}",
            lagging.join(", ")
        ),
        ReconcileDecision::Ahead { hosts } => format!(
            "AHEAD of the latest release: {}; nothing rolled out, never downgraded",
            hosts.join(", ")
        ),
        ReconcileDecision::Rollout { lagging } => format!(
            "{} lag; {}",
            lagging.join(", "),
            if apply {
                "ran the verified fleet rollout to these host classes only"
            } else {
                "a rollout is due (pass --apply)"
            }
        ),
        ReconcileDecision::Unknown { reason } => format!("UNKNOWN: {reason}"),
        ReconcileDecision::ControllerBusy => {
            "another rollout holds the controller lock; skipped".to_owned()
        }
    }
}

/// Doctor section: each configured host's installed version against the
/// latest published release, flagged when it lags past the soak, plus any tag
/// fleet-reconcile has stopped retrying.
#[allow(clippy::too_many_lines)] // One read-only report assembled in display order.
pub(in crate::app) fn fleet_version_doctor_section(
    runtime_paths: &RuntimePaths,
    cwd: &Path,
    soak_minutes: u64,
) -> BTreeMap<String, DoctorEntry> {
    let entry =
        |ok: bool, version: Option<String>, detail: Option<String>, error: Option<String>| {
            DoctorEntry {
                ok,
                version,
                detail,
                error,
            }
        };
    let mut rows = BTreeMap::new();
    let config = match LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
    {
        Ok(config) => config,
        Err(error) => {
            rows.insert(
                "config".to_owned(),
                entry(false, None, None, Some(error.to_string())),
            );
            return rows;
        }
    };
    let classes = match parse_host_classes(&config.data) {
        Ok(classes) if !classes.is_empty() => classes,
        Ok(_) => {
            rows.insert(
                "fleet".to_owned(),
                entry(
                    true,
                    Some("no host classes configured".to_owned()),
                    None,
                    None,
                ),
            );
            return rows;
        }
        Err(error) => {
            rows.insert("config".to_owned(), entry(false, None, None, Some(error)));
            return rows;
        }
    };
    if let Ok(ledger) = reconcile::read_ledger(&runtime_paths.state_dir) {
        for (class, quarantine) in &ledger.quarantined {
            rows.insert(
                format!("quarantined:{class}"),
                entry(
                    false,
                    Some(quarantine.tag.clone()),
                    Some(format!(
                        "rollback failed {}: {}; fix the host, then `shipyard runner fleet-reconcile --clear-host {class}`",
                        quarantine.since, quarantine.reason
                    )),
                    Some("needs an operator".to_owned()),
                ),
            );
        }
    }
    for (tag, attempts) in terminal_tags(&runtime_paths.state_dir) {
        rows.insert(
            format!("reconcile:{tag}"),
            entry(
                false,
                Some(format!("{} attempt(s)", attempts.attempts)),
                attempts.terminal.clone(),
                Some("fleet-reconcile stopped retrying this release".to_owned()),
            ),
        );
    }
    let latest = latest_release(&config, cwd);
    let now = Utc::now();
    match &latest {
        Ok(latest) => rows.insert(
            "latest-release".to_owned(),
            entry(
                true,
                Some(latest.tag.clone()),
                Some(format!("published {}", latest.published_at)),
                None,
            ),
        ),
        Err(error) => rows.insert(
            "latest-release".to_owned(),
            entry(false, None, None, Some(format!("UNKNOWN: {error}"))),
        ),
    };
    let mut hosts = classes
        .iter()
        .map(reconcile::probe_host_version)
        .collect::<Vec<_>>();
    if let Ok(latest) = &latest {
        reconcile::classify_hosts(latest, &mut hosts);
    }
    for host in hosts {
        if host.declares_host_classes == Some(true) {
            rows.insert(
                format!("controller:{}", host.host_class),
                entry(
                    false,
                    None,
                    Some(format!(
                        "{} declares [host_class.*] in its own machine-global config; exactly one \
                         Mac may be the fleet controller, or two reconcilers will race the same hosts",
                        host.host_class
                    )),
                    Some("second controller".to_owned()),
                ),
            );
        }
        let row = fleet_version_row(&host, latest.as_ref().ok(), now, soak_minutes);
        rows.insert(format!("host:{}", host.host_class), row);
    }
    rows
}

fn terminal_tags(state_dir: &Path) -> Vec<(String, reconcile::TagAttempts)> {
    reconcile::read_ledger(state_dir)
        .map(|ledger| {
            ledger
                .tags
                .into_iter()
                .filter(|(_, attempts)| attempts.terminal.is_some())
                .collect()
        })
        .unwrap_or_default()
}

/// Doctor row for one host: ok when current, or behind but still soaking;
/// failed when it lags past the soak, runs ahead of the release, or its
/// version is unknown.
pub(super) fn fleet_version_row(
    host: &HostVersion,
    latest: Option<&PublishedRelease>,
    now: DateTime<Utc>,
    soak_minutes: u64,
) -> DoctorEntry {
    let soak = chrono::Duration::minutes(i64::try_from(soak_minutes).unwrap_or(30));
    let ahead = latest.is_some_and(|latest| {
        matches!(
            (
                host.version.as_deref().and_then(reconcile::parse_version),
                reconcile::parse_version(&latest.tag)
            ),
            (Some(installed), Some(target)) if installed > target
        )
    });
    let (ok, detail, error) = match (host.lagging, latest) {
        (Some(false), Some(latest)) if ahead => (
            false,
            Some(format!(
                "runs ahead of the latest release {}; fleet-reconcile will not act",
                latest.tag
            )),
            Some("ahead".to_owned()),
        ),
        (Some(false), _) => (true, None, None),
        (Some(true), Some(latest)) if now >= latest.published_at + soak => (
            false,
            Some(format!(
                "lags {} past the {soak_minutes}-minute soak; run `shipyard runner fleet-reconcile --apply`",
                latest.tag
            )),
            Some("lagging".to_owned()),
        ),
        (Some(true), Some(latest)) => (
            true,
            Some(format!(
                "behind {}, still inside the soak window",
                latest.tag
            )),
            None,
        ),
        _ => (
            false,
            None,
            Some(format!(
                "UNKNOWN: {}",
                host.error.as_deref().unwrap_or("latest release unreadable")
            )),
        ),
    };
    DoctorEntry {
        ok,
        version: host.version.clone(),
        detail,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_tags_surface_as_doctor_findings() {
        let temp = tempfile::tempdir().expect("temp");
        reconcile::record_attempt(temp.path(), "v0.208.0", Utc::now()).expect("attempt");
        reconcile::mark_terminal(temp.path(), "v0.208.0", "gave up after 3 attempts")
            .expect("terminal");
        reconcile::record_attempt(temp.path(), "v0.209.0", Utc::now()).expect("attempt");
        let tags = terminal_tags(temp.path());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].0, "v0.208.0");
    }
}
