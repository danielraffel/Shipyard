//! The daemon's scheduled GitHub metrics import (`[metrics.import]`).
//!
//! `shipyard metrics import github` only ever ran when someone typed it, so the
//! store silently aged until every verdict read "insufficient samples". The
//! daemon now runs the same import on a cadence when the machine-global config
//! opts in:
//!
//! ```toml
//! [metrics.import]
//! enabled = true            # default false: nothing is read or written
//! interval_minutes = 60     # per-repository cadence (minimum 5)
//! repos = ["Generous-Corp/pulp"]   # default: the repositories the daemon serves
//! workflow = "build.yml"    # optional; every workflow when unset
//! limit = 20                # recent runs per pass (1-100)
//! ```
//!
//! The daemon ticks this job every [`CHECK_INTERVAL_SECS`]; each pass re-reads
//! config (so a toggle needs no restart) and imports a repository only when
//! its last *attempt* — persisted in `metrics/import-schedule.json` — is older
//! than the interval. Recording attempts rather than successes keeps a failing
//! import from retrying every tick. A pass is bounded: one `gh` read for the
//! run list plus one per run, each under [`GH_TIMEOUT`], on its own thread.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::MetricsStore;
use super::github_import::{GithubImportRequest, import_github};
use crate::config::LoadedConfig;

/// How often the daemon asks whether an import is due.
pub const CHECK_INTERVAL_SECS: u64 = 5 * 60;
/// Default per-repository cadence.
pub const DEFAULT_INTERVAL_MINUTES: i64 = 60;
/// Smallest accepted cadence.
pub const MIN_INTERVAL_MINUTES: i64 = 5;
/// Default runs per pass.
pub const DEFAULT_LIMIT: u32 = 20;
/// Per-`gh`-call deadline.
pub const GH_TIMEOUT: StdDuration = StdDuration::from_secs(30);

/// `[metrics.import]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportConfig {
    pub enabled: bool,
    pub interval: Duration,
    pub repos: Vec<String>,
    pub workflow: Option<String>,
    pub limit: u32,
}

impl Default for ImportConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Duration::minutes(DEFAULT_INTERVAL_MINUTES),
            repos: Vec::new(),
            workflow: None,
            limit: DEFAULT_LIMIT,
        }
    }
}

impl ImportConfig {
    /// Read `[metrics.import]`; every key is optional and `enabled` defaults
    /// to `false`.
    #[must_use]
    pub fn from_config(config: &LoadedConfig) -> Self {
        let mut out = Self {
            enabled: config
                .get("metrics.import.enabled")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
            ..Self::default()
        };
        if let Some(minutes) = config
            .get("metrics.import.interval_minutes")
            .and_then(toml::Value::as_integer)
        {
            out.interval = Duration::minutes(minutes.max(MIN_INTERVAL_MINUTES));
        }
        if let Some(repos) = config
            .get("metrics.import.repos")
            .and_then(toml::Value::as_array)
        {
            out.repos = repos
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect();
        }
        out.workflow = config.get_str("metrics.import.workflow").map(str::to_owned);
        if let Some(limit) = config
            .get("metrics.import.limit")
            .and_then(toml::Value::as_integer)
        {
            out.limit = u32::try_from(limit.clamp(1, 100)).unwrap_or(DEFAULT_LIMIT);
        }
        out
    }

    /// Repositories to import: configured ones, else the daemon's own.
    #[must_use]
    pub fn repos_or(&self, daemon_repos: &[String]) -> Vec<String> {
        if self.repos.is_empty() {
            daemon_repos.to_vec()
        } else {
            self.repos.clone()
        }
    }
}

/// Whether a repository is due: never attempted, or last attempted at least
/// `interval` ago. A clock that went backwards counts as due.
#[must_use]
pub fn import_due(
    last_attempt: Option<DateTime<Utc>>,
    interval: Duration,
    now: DateTime<Utc>,
) -> bool {
    last_attempt.is_none_or(|last| now < last || now - last >= interval)
}

/// Persisted per-repository schedule state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleState {
    #[serde(default)]
    pub repos: BTreeMap<String, RepoScheduleState>,
}

/// One repository's last attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoScheduleState {
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_imported: Option<usize>,
    pub last_error: Option<String>,
}

/// `metrics/import-schedule.json` under the state directory.
#[must_use]
pub fn schedule_path(state_dir: &Path) -> PathBuf {
    state_dir.join("metrics").join("import-schedule.json")
}

fn load_state(path: &Path) -> ScheduleState {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &ScheduleState) -> Result<(), String> {
    let parent = path.parent().ok_or("schedule path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(state).map_err(|error| error.to_string())?;
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

/// Outcome for one repository in a pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepoImportOutcome {
    pub repo: String,
    /// `imported`, `not_due` or `failed`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imported: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One daemon pass.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ImportPass {
    pub enabled: bool,
    pub repos: Vec<RepoImportOutcome>,
    pub errors: Vec<String>,
}

impl ImportPass {
    /// Whether anything happened worth broadcasting.
    #[must_use]
    pub fn did_work(&self) -> bool {
        !self.errors.is_empty() || self.repos.iter().any(|repo| repo.status != "not_due")
    }
}

/// Runs one `gh` argv against a repository and returns its parsed JSON.
pub type RepoGhRunner<'a> = dyn FnMut(&str, &[String]) -> Result<Value, String> + 'a;

/// Run one pass with an injected `gh` runner (per repository).
pub fn run_pass(
    config: &ImportConfig,
    state_dir: &Path,
    daemon_repos: &[String],
    now: DateTime<Utc>,
    gh_for_repo: &mut RepoGhRunner<'_>,
) -> ImportPass {
    if !config.enabled {
        return ImportPass::default();
    }
    let mut pass = ImportPass {
        enabled: true,
        ..ImportPass::default()
    };
    let repos = config.repos_or(daemon_repos);
    if repos.is_empty() {
        pass.errors
            .push("[metrics.import] is enabled but names no repository".to_owned());
        return pass;
    }
    let path = schedule_path(state_dir);
    let mut state = load_state(&path);
    let due: Vec<String> = repos
        .into_iter()
        .filter(|repo| {
            let last = state
                .repos
                .get(repo)
                .and_then(|entry| entry.last_attempt_at);
            let due = import_due(last, config.interval, now);
            if !due {
                pass.repos.push(RepoImportOutcome {
                    repo: repo.clone(),
                    status: "not_due",
                    imported: None,
                    error: None,
                });
            }
            due
        })
        .collect();
    if due.is_empty() {
        return pass;
    }
    let store = match MetricsStore::open(state_dir) {
        Ok(store) => store,
        Err(error) => {
            pass.errors.push(format!("metrics store: {error}"));
            return pass;
        }
    };
    for repo in due {
        let request = GithubImportRequest {
            repo: repo.clone(),
            project: None,
            workflow: config.workflow.clone(),
            branch: None,
            limit: config.limit,
        };
        let mut gh = |argv: &[String]| gh_for_repo(&repo, argv);
        let result = import_github(&store, &request, &mut gh, &|message| message);
        let entry = state.repos.entry(repo.clone()).or_default();
        entry.last_attempt_at = Some(now);
        match result {
            Ok(imported) => {
                entry.last_success_at = Some(now);
                entry.last_imported = Some(imported);
                entry.last_error = None;
                pass.repos.push(RepoImportOutcome {
                    repo,
                    status: "imported",
                    imported: Some(imported),
                    error: None,
                });
            }
            Err(error) => {
                entry.last_error = Some(error.clone());
                pass.repos.push(RepoImportOutcome {
                    repo,
                    status: "failed",
                    imported: None,
                    error: Some(error),
                });
            }
        }
    }
    if let Err(error) = save_state(&path, &state) {
        pass.errors.push(format!("import schedule state: {error}"));
    }
    pass
}

/// The daemon entry point: machine-global config, the configured GitHub
/// client, and [`run_pass`].
#[must_use]
pub fn daemon_pass(
    global_dir: &Path,
    state_dir: &Path,
    daemon_repos: &[String],
    now: DateTime<Utc>,
) -> ImportPass {
    let config = match LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf()) {
        Ok(config) => config,
        Err(error) => {
            return ImportPass {
                errors: vec![format!("config: {error}")],
                ..ImportPass::default()
            };
        }
    };
    let import = ImportConfig::from_config(&config);
    if !import.enabled {
        return ImportPass::default();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut gh = |repo: &str, argv: &[String]| -> Result<Value, String> {
        let actions =
            crate::cloud::GitHubActions::from_loaded_config(&cwd, &config).with_repo_override(repo);
        let text = actions
            .run_gh_with_timeout_env(argv, GH_TIMEOUT, &[("GH_REPO", repo)])
            .map_err(|error| error.to_string())?;
        serde_json::from_str(&text).map_err(|error| format!("gh JSON parse failed: {error}"))
    };
    run_pass(&import, state_dir, daemon_repos, now, &mut gh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn enabled(repos: &[&str]) -> ImportConfig {
        ImportConfig {
            enabled: true,
            repos: repos.iter().map(|repo| (*repo).to_owned()).collect(),
            ..ImportConfig::default()
        }
    }

    fn fake_gh(calls: &mut Vec<String>) -> impl FnMut(&str, &[String]) -> Result<Value, String> {
        move |repo, argv| {
            calls.push(format!("{repo} {}", argv[3]));
            if argv[3].ends_with("/runs") {
                Ok(json!({"workflow_runs": [{"id": 7, "pull_requests": []}]}))
            } else {
                Ok(
                    json!({"jobs": [{"id": 1, "name": "macos", "conclusion": "success",
                    "started_at": "2026-09-30T10:00:00Z",
                    "completed_at": "2026-09-30T10:05:00Z"}]}),
                )
            }
        }
    }

    #[test]
    fn due_only_after_the_interval() {
        let now = at("2026-09-30T12:00:00Z");
        let hour = Duration::hours(1);
        assert!(import_due(None, hour, now));
        assert!(!import_due(Some(at("2026-09-30T11:30:00Z")), hour, now));
        assert!(import_due(Some(at("2026-09-30T11:00:00Z")), hour, now));
        // A clock that went backwards must not wedge the schedule.
        assert!(import_due(Some(at("2026-10-01T00:00:00Z")), hour, now));
    }

    #[test]
    fn config_off_imports_nothing_and_touches_nothing() {
        let state = tempfile::tempdir().unwrap();
        let mut calls = Vec::new();
        let pass = run_pass(
            &ImportConfig::default(),
            state.path(),
            &["Generous-Corp/pulp".to_owned()],
            Utc::now(),
            &mut fake_gh(&mut calls),
        );
        assert_eq!(pass, ImportPass::default());
        assert!(calls.is_empty());
        assert!(!state.path().join("metrics").exists());
    }

    #[test]
    fn daemon_pass_without_config_is_disabled() {
        let global = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let pass = daemon_pass(global.path(), state.path(), &["o/r".to_owned()], Utc::now());
        assert!(!pass.enabled);
        assert!(pass.errors.is_empty());
        assert!(!state.path().join("metrics").exists());
    }

    #[test]
    fn an_enabled_pass_imports_then_waits_for_the_interval() {
        let state = tempfile::tempdir().unwrap();
        let config = enabled(&["Generous-Corp/pulp"]);
        let now = at("2026-09-30T12:00:00Z");
        let mut calls = Vec::new();
        let first = run_pass(&config, state.path(), &[], now, &mut fake_gh(&mut calls));
        assert_eq!(first.repos[0].status, "imported");
        assert_eq!(first.repos[0].imported, Some(1));
        assert_eq!(calls.len(), 2);

        let mut calls = Vec::new();
        let early = run_pass(
            &config,
            state.path(),
            &[],
            now + Duration::minutes(10),
            &mut fake_gh(&mut calls),
        );
        assert_eq!(early.repos[0].status, "not_due");
        assert!(!early.did_work());
        assert!(calls.is_empty());

        let mut calls = Vec::new();
        let later = run_pass(
            &config,
            state.path(),
            &[],
            now + Duration::minutes(61),
            &mut fake_gh(&mut calls),
        );
        assert_eq!(later.repos[0].status, "imported");
        // The store now answers a slug lookup for the imported rows.
        let store = MetricsStore::open(state.path()).unwrap();
        assert_eq!(store.list(Some("Generous-Corp/pulp"), 10).unwrap().len(), 1);
    }

    #[test]
    fn a_failed_import_still_waits_before_retrying() {
        let state = tempfile::tempdir().unwrap();
        let config = enabled(&["Generous-Corp/pulp"]);
        let now = at("2026-09-30T12:00:00Z");
        let mut failing =
            |_: &str, _: &[String]| -> Result<Value, String> { Err("HTTP 502".to_owned()) };
        let pass = run_pass(&config, state.path(), &[], now, &mut failing);
        assert_eq!(pass.repos[0].status, "failed");
        assert_eq!(pass.repos[0].error.as_deref(), Some("HTTP 502"));
        let retry = run_pass(
            &config,
            state.path(),
            &[],
            now + Duration::minutes(1),
            &mut failing,
        );
        assert_eq!(retry.repos[0].status, "not_due");
        let saved = load_state(&schedule_path(state.path()));
        assert_eq!(
            saved.repos["Generous-Corp/pulp"].last_error.as_deref(),
            Some("HTTP 502")
        );
    }

    #[test]
    fn enabled_without_repositories_reports_instead_of_guessing() {
        let state = tempfile::tempdir().unwrap();
        let mut calls = Vec::new();
        let pass = run_pass(
            &enabled(&[]),
            state.path(),
            &[],
            Utc::now(),
            &mut fake_gh(&mut calls),
        );
        assert!(pass.enabled);
        assert_eq!(pass.errors.len(), 1);
        assert!(calls.is_empty());
    }

    #[test]
    fn config_keys_parse_and_clamp() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[metrics.import]\nenabled = true\ninterval_minutes = 1\nrepos = [\"a/b\"]\n\
             workflow = \"build.yml\"\nlimit = 500\n",
        )
        .unwrap();
        let config = LoadedConfig::load_machine_global_from_dir(dir.path().to_path_buf()).unwrap();
        let import = ImportConfig::from_config(&config);
        assert!(import.enabled);
        assert_eq!(import.interval, Duration::minutes(MIN_INTERVAL_MINUTES));
        assert_eq!(import.repos, vec!["a/b".to_owned()]);
        assert_eq!(import.workflow.as_deref(), Some("build.yml"));
        assert_eq!(import.limit, 100);
    }
}
