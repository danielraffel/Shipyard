//! Whether this host's pr-watch passes are still completing.
//!
//! Hand-back, sticky comments, and the digest all run inside the daemon's
//! pass on whichever host has `[pr_watch] enabled = true`, usually one. If
//! that pass stops completing (daemon down, auth broken, rate limited), every
//! channel goes quiet at once and nothing else notices. A pass that completes
//! stamps `last_scan_at` in the repository's ledger, so a stale stamp is the
//! signal. `shipyard pr-watch liveness` and `shipyard doctor` read it here;
//! `shipyard doctor --fleet` asks each configured host.

use std::fmt::Write as _;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::ledger;
use super::scan::WatchConfig;

/// Minutes without a completed pass before a scanning host is stale: three
/// missed 15-minute passes.
pub const DEFAULT_STALE_AFTER_MINUTES: i64 = 45;

/// One repository's last completed pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoLiveness {
    /// `OWNER/REPO`.
    pub repo: String,
    /// When the last pass completed, if one ever did.
    pub last_scan_at: Option<DateTime<Utc>>,
    /// Whole minutes since then.
    pub age_minutes: Option<i64>,
    /// Completed within the stale threshold.
    pub fresh: bool,
    /// Why the ledger could not be read, if so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The last pass's coverage: handed pull requests no rule accounts for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<super::coverage::CoverageSummary>,
}

/// This host's pr-watch liveness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Liveness {
    /// `[pr_watch] enabled` on this host: whether it is a scanning host.
    pub enabled: bool,
    /// The threshold used.
    pub stale_after_minutes: i64,
    /// One row per configured (or ledgered) repository.
    pub repos: Vec<RepoLiveness>,
}

impl Liveness {
    /// A scanning host whose every repository passed recently. A host that
    /// does not scan is not stale.
    #[must_use]
    pub fn healthy(&self) -> bool {
        !self.enabled || (!self.repos.is_empty() && self.repos.iter().all(|repo| repo.fresh))
    }
}

/// Read this host's liveness. Repositories come from `[pr_watch] repos`; when
/// that is empty (the daemon then scans its `--repo` list), every ledger under
/// `<state_dir>/pr-watch/` counts.
#[must_use]
pub fn read(
    watch: &WatchConfig,
    state_dir: &Path,
    now: DateTime<Utc>,
    stale_after: Duration,
) -> Liveness {
    let mut repos = Vec::new();
    if watch.enabled {
        if watch.repos.is_empty() {
            for ledger in ledgers_in(&state_dir.join("pr-watch"), &watch.base) {
                let mut found = row(&ledger.repo, Ok(ledger.last_scan_at), now, stale_after);
                found.coverage = ledger.coverage;
                repos.push(found);
            }
        } else {
            for repo in &watch.repos {
                let path = ledger::default_path(state_dir, repo, &watch.base);
                let loaded = if path.exists() {
                    ledger::load(&path, repo, &watch.base).map(Some)
                } else {
                    Ok(None)
                };
                let stamp = loaded
                    .as_ref()
                    .map(|ledger| ledger.as_ref().and_then(|l| l.last_scan_at))
                    .map_err(Clone::clone);
                let mut found = row(repo, stamp, now, stale_after);
                found.coverage = loaded.ok().flatten().and_then(|ledger| ledger.coverage);
                repos.push(found);
            }
        }
    }
    Liveness {
        enabled: watch.enabled,
        stale_after_minutes: stale_after.num_minutes(),
        repos,
    }
}

fn row(
    repo: &str,
    stamp: Result<Option<DateTime<Utc>>, String>,
    now: DateTime<Utc>,
    stale_after: Duration,
) -> RepoLiveness {
    match stamp {
        Ok(last) => RepoLiveness {
            repo: repo.to_owned(),
            last_scan_at: last,
            age_minutes: last.map(|at| (now - at).num_minutes()),
            fresh: last.is_some_and(|at| now - at <= stale_after),
            error: None,
            coverage: None,
        },
        Err(error) => RepoLiveness {
            repo: repo.to_owned(),
            last_scan_at: None,
            age_minutes: None,
            fresh: false,
            error: Some(error),
            coverage: None,
        },
    }
}

fn ledgers_in(dir: &Path, base: &str) -> Vec<ledger::Ledger> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<ledger::Ledger> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("json")
                && !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".events.json"))
        })
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|raw| serde_json::from_str::<ledger::Ledger>(&raw).ok())
        .filter(|ledger| ledger.base == base)
        .collect();
    found.sort_by(|a, b| a.repo.cmp(&b.repo));
    found
}

/// One line per repository.
#[must_use]
pub fn render(liveness: &Liveness) -> String {
    if !liveness.enabled {
        return "pr-watch: not a scanning host ([pr_watch] enabled is off)\n".to_owned();
    }
    if liveness.repos.is_empty() {
        return "pr-watch: STALE: enabled, but no repository has a ledger yet\n".to_owned();
    }
    let mut out = String::new();
    for repo in &liveness.repos {
        out.push_str(&line(repo, liveness.stale_after_minutes));
        out.push('\n');
        if let Some((_, coverage)) = coverage_line(repo) {
            out.push_str(&coverage);
            out.push('\n');
        }
    }
    out
}

/// The coverage line for one repository: counts, then each gap with its
/// reason. `None` before a pass has recorded coverage.
#[must_use]
pub fn coverage_line(repo: &RepoLiveness) -> Option<(bool, String)> {
    let coverage = repo.coverage.as_ref()?;
    let mut text = format!(
        "coverage {}: {} progressing, {} flagged, {} held, {} unaccounted",
        repo.repo,
        coverage.progressing,
        coverage.flagged,
        coverage.held,
        coverage.gaps.len()
    );
    for gap in &coverage.gaps {
        let _ = write!(text, "\n  #{}: {}", gap.pr, gap.reason);
    }
    Some((coverage.gaps.is_empty(), text))
}

/// The verdict for one repository, as `render` and `doctor` print it.
#[must_use]
pub fn line(repo: &RepoLiveness, stale_after_minutes: i64) -> String {
    let state = if repo.fresh { "ok" } else { "STALE" };
    match (&repo.error, repo.last_scan_at, repo.age_minutes) {
        (Some(error), _, _) => format!("pr-watch {}: STALE: ledger unreadable: {error}", repo.repo),
        (None, Some(at), Some(age)) => format!(
            "pr-watch {}: {state}: last completed pass {} ({age} min ago; stale after {stale_after_minutes})",
            repo.repo,
            at.format("%Y-%m-%dT%H:%MZ")
        ),
        _ => format!("pr-watch {}: STALE: no pass has ever completed", repo.repo),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 9, 0, 0, 0).unwrap() + Duration::minutes(minute)
    }

    fn watch(enabled: bool, repos: &[&str]) -> WatchConfig {
        WatchConfig {
            enabled,
            repos: repos.iter().map(|r| (*r).to_owned()).collect(),
            ..WatchConfig::default()
        }
    }

    fn write_ledger(state: &Path, repo: &str, last: Option<DateTime<Utc>>) {
        let path = ledger::default_path(state, repo, "main");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut ledger = ledger::Ledger::new(repo, "main");
        ledger.last_scan_at = last;
        ledger::save(&path, &ledger).unwrap();
    }

    #[test]
    fn a_scanning_host_is_stale_after_the_threshold_and_a_quiet_one_never_is() {
        let dir = tempfile::tempdir().unwrap();
        write_ledger(dir.path(), "o/fresh", Some(at(0)));
        write_ledger(dir.path(), "o/old", Some(at(-60)));
        let stale_after = Duration::minutes(DEFAULT_STALE_AFTER_MINUTES);
        let live = read(&watch(true, &["o/fresh"]), dir.path(), at(45), stale_after);
        assert!(live.healthy(), "{live:?}");
        let late = read(&watch(true, &["o/fresh"]), dir.path(), at(46), stale_after);
        assert!(!late.healthy());
        assert!(
            render(&late).contains("STALE: last completed pass"),
            "{}",
            render(&late)
        );
        let mixed = read(
            &watch(true, &["o/fresh", "o/old"]),
            dir.path(),
            at(10),
            stale_after,
        );
        assert!(!mixed.healthy());
        let never = read(&watch(true, &["o/none"]), dir.path(), at(10), stale_after);
        assert!(!never.healthy());
        assert!(render(&never).contains("no pass has ever completed"));
        // Not a scanning host: nothing to be stale about.
        let quiet = read(&watch(false, &["o/old"]), dir.path(), at(500), stale_after);
        assert!(quiet.healthy());
        assert!(quiet.repos.is_empty());
    }

    #[test]
    fn without_configured_repos_every_ledger_counts() {
        let dir = tempfile::tempdir().unwrap();
        let stale_after = Duration::minutes(DEFAULT_STALE_AFTER_MINUTES);
        let empty = read(&watch(true, &[]), dir.path(), at(0), stale_after);
        assert!(!empty.healthy(), "enabled with no ledger is not healthy");
        write_ledger(dir.path(), "o/a", Some(at(0)));
        write_ledger(dir.path(), "o/b", Some(at(-100)));
        let live = read(&watch(true, &[]), dir.path(), at(5), stale_after);
        let repos: Vec<(&str, bool)> = live
            .repos
            .iter()
            .map(|r| (r.repo.as_str(), r.fresh))
            .collect();
        assert_eq!(repos, vec![("o/a", true), ("o/b", false)]);
    }
}
