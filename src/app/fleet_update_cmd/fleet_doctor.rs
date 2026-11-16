//! `shipyard doctor --fleet`: each configured host's installed version against
//! the latest published release, plus the pr-watch scan rows.
//!
//! Every host is enumerated from the machine-global `[host_class.<name>]`
//! configuration. No host is named in code, so adding a machine is adding its
//! host class (and its tartci profile); it is then probed and reported like
//! every other host.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};

use super::fleet_state::{self, HostVersion, PublishedRelease};
use super::{GitHubReleaseAuthorityVerifier, release_authority};
use crate::capacity::parse_host_classes;
use crate::config::LoadedConfig;
use crate::doctor::DoctorEntry;
use crate::paths::RuntimePaths;

fn latest_release(config: &LoadedConfig, cwd: &Path) -> Result<PublishedRelease, String> {
    let repository = release_authority::release_repository()?;
    GitHubReleaseAuthorityVerifier::new(config, cwd)
        .api_json(&format!("repos/{repository}/releases/latest"))
        .and_then(|value| fleet_state::parse_latest_release(&value))
}

/// Doctor section: each configured host's installed version against the
/// latest published release, flagged when it lags past the soak, plus any
/// quarantined host and any tag a failed rollback made terminal.
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
    if let Ok(ledger) = fleet_state::read_ledger(&runtime_paths.state_dir) {
        for (class, quarantine) in &ledger.quarantined {
            rows.insert(
                format!("quarantined:{class}"),
                entry(
                    false,
                    Some(quarantine.tag.clone()),
                    Some(format!(
                        "rollback failed {}: {}; fix the host, then `shipyard runner fleet-update --clear-host {class}`",
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
                Some(tag.clone()),
                attempts.terminal.clone(),
                Some("this release is terminal for fleet rollouts".to_owned()),
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
        .map(fleet_state::probe_host_version)
        .collect::<Vec<_>>();
    if let Ok(latest) = &latest {
        fleet_state::classify_hosts(latest, &mut hosts);
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
                         Mac may be the fleet controller, or two controllers will race the same hosts",
                        host.host_class
                    )),
                    Some("second controller".to_owned()),
                ),
            );
        }
        let row = fleet_version_row(&host, latest.as_ref().ok(), now, soak_minutes);
        rows.insert(format!("host:{}", host.host_class), row);
    }
    let answers: Vec<(String, Result<crate::pr_watch::liveness::Liveness, String>)> = classes
        .iter()
        .map(|class| {
            (
                class.class.clone(),
                fleet_state::probe_pr_watch_liveness(class),
            )
        })
        .collect();
    rows.extend(pr_watch_fleet_rows(&answers));
    rows
}

/// Fleet rows for pr-watch: one per scanning host, plus `pr-watch:fleet`,
/// which fails when no host reports a pass completed within its threshold.
/// pr-watch usually runs on one host, so its silence is otherwise invisible.
/// A host that could not be asked is reported but does not fail on its own.
pub(super) fn pr_watch_fleet_rows(
    answers: &[(String, Result<crate::pr_watch::liveness::Liveness, String>)],
) -> BTreeMap<String, DoctorEntry> {
    let mut rows = BTreeMap::new();
    let mut fresh_scanners = Vec::new();
    let mut stale_scanners = Vec::new();
    for (class, answer) in answers {
        match answer {
            Ok(liveness) if liveness.enabled => {
                let detail = if liveness.repos.is_empty() {
                    "enabled, but no repository has a ledger yet".to_owned()
                } else {
                    liveness
                        .repos
                        .iter()
                        .map(|repo| {
                            let mut text =
                                crate::pr_watch::liveness::line(repo, liveness.stale_after_minutes);
                            if let Some((_, coverage)) =
                                crate::pr_watch::liveness::coverage_line(repo)
                            {
                                text.push('\n');
                                text.push_str(&coverage);
                            }
                            text
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let gaps: usize = liveness
                    .repos
                    .iter()
                    .filter_map(|repo| repo.coverage.as_ref())
                    .map(|coverage| coverage.gaps.len())
                    .sum();
                let healthy = liveness.healthy();
                if healthy {
                    fresh_scanners.push(class.clone());
                } else {
                    stale_scanners.push(class.clone());
                }
                let error = if !healthy {
                    Some(
                        "pr-watch passes stopped completing; hand-back, comments and digest are silent"
                            .to_owned(),
                    )
                } else if gaps > 0 {
                    Some(format!(
                        "{gaps} handed pull request(s) no rule accounts for (flag 8 calls their owners)"
                    ))
                } else {
                    None
                };
                rows.insert(
                    format!("pr-watch:{class}"),
                    DoctorEntry {
                        ok: error.is_none(),
                        version: Some(if healthy { "scanning" } else { "STALE" }.to_owned()),
                        detail: Some(detail),
                        error,
                    },
                );
            }
            Ok(_) => {}
            Err(error) => {
                rows.insert(
                    format!("pr-watch:{class}"),
                    DoctorEntry {
                        ok: true,
                        version: Some("unknown".to_owned()),
                        detail: Some(error.clone()),
                        error: None,
                    },
                );
            }
        }
    }
    let fleet = if fresh_scanners.is_empty() {
        DoctorEntry {
            ok: false,
            version: None,
            detail: Some(if stale_scanners.is_empty() {
                "no configured host reports [pr_watch] enabled".to_owned()
            } else {
                format!("only stale scanners: {}", stale_scanners.join(", "))
            }),
            error: Some("no host is completing pr-watch passes".to_owned()),
        }
    } else {
        DoctorEntry {
            ok: stale_scanners.is_empty(),
            version: Some(format!("scanning on {}", fresh_scanners.join(", "))),
            detail: (!stale_scanners.is_empty())
                .then(|| format!("stale: {}", stale_scanners.join(", "))),
            error: None,
        }
    };
    rows.insert("pr-watch:fleet".to_owned(), fleet);
    rows
}

fn terminal_tags(state_dir: &Path) -> Vec<(String, fleet_state::TagRecord)> {
    fleet_state::read_ledger(state_dir)
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
                host.version.as_deref().and_then(fleet_state::parse_version),
                fleet_state::parse_version(&latest.tag)
            ),
            (Some(installed), Some(target)) if installed > target
        )
    });
    let (ok, detail, error) = match (host.lagging, latest) {
        (Some(false), Some(latest)) if ahead => (
            false,
            Some(format!("runs ahead of the latest release {}", latest.tag)),
            Some("ahead".to_owned()),
        ),
        (Some(false), _) => (true, None, None),
        (Some(true), Some(latest)) if now >= latest.published_at + soak => (
            false,
            Some(format!(
                "lags {} past the {soak_minutes}-minute soak; TartCI self-update rolls hosts forward (`tartci fleet-macos tool-freshness` shows each host), or run `shipyard runner fleet-update --all-hosts --lagging-only --apply --to <tag>`",
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
        fleet_state::update_ledger(temp.path(), |ledger| {
            ledger
                .tags
                .entry("v0.208.0".to_owned())
                .or_default()
                .terminal = Some("rollback failed on m3: gone".to_owned());
            ledger.tags.entry("v0.209.0".to_owned()).or_default();
        })
        .expect("ledger");
        let tags = terminal_tags(temp.path());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].0, "v0.208.0");
    }
}
