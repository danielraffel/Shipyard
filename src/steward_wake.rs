//! Event-driven, single-authority merge-steward wakeups.
//!
//! The daemon already receives signed repository webhooks. This module turns
//! terminal CI/PR transitions into coalesced steward passes on the one machine
//! selected by `merge_queue.mutation_machine`. A low-frequency reconcile is a
//! safety net for events missed while the authority host was offline.

#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Value, json};

use crate::config::LoadedConfig;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::merge_queue_control::authority_status;
use crate::process::ProcessTree;

const EVENT_DEBOUNCE: Duration = Duration::from_secs(2);
const RECONCILE_INTERVAL: Duration = Duration::from_mins(30);
const FAILURE_RETRY_DELAY: Duration = Duration::from_secs(30);
const DEFAULT_BRANCH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_LOG_BYTES: u64 = 1024 * 1024;

struct ActiveWake {
    repo: String,
    child: ProcessTree,
}

/// Live event-to-steward bridge owned by the daemon.
pub struct StewardWakeRuntime {
    scheduler: StewardWakeScheduler,
    active: Option<ActiveWake>,
    default_branches: BTreeMap<String, String>,
    binary: PathBuf,
    cwd: PathBuf,
    state_dir: PathBuf,
    global_dir: PathBuf,
    mode: RuntimeMode,
}

impl Drop for StewardWakeRuntime {
    fn drop(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active.child.terminate();
        }
    }
}

impl StewardWakeRuntime {
    /// Enable wakeups only on the trusted mutation authority. All other hosts
    /// remain passive webhook consumers.
    #[must_use]
    pub fn for_authority(
        repos: &[String],
        state_dir: &Path,
        global_dir: &Path,
        cwd: &Path,
        mode: RuntimeMode,
    ) -> Option<Self> {
        if cfg!(test) || mode != RuntimeMode::Shipyard || repos.is_empty() {
            return None;
        }
        let authority = authority_status(state_dir, cwd, mode, global_dir).ok()?;
        if authority.get("authority_matches").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let binary = std::env::current_exe().ok()?;
        Some(Self {
            scheduler: StewardWakeScheduler::new(repos, Instant::now()),
            active: None,
            default_branches: BTreeMap::new(),
            binary,
            cwd: cwd.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            global_dir: global_dir.to_path_buf(),
            mode,
        })
    }

    /// Coalesce a relevant webhook into a repository-scoped steward pass.
    pub fn observe(&mut self, event: &Value, now: Instant) {
        self.scheduler.observe(event, now);
    }

    /// Advance the singleflight worker without blocking the daemon event loop.
    pub fn tick(&mut self, now: Instant) {
        self.scheduler.schedule_periodic(now);
        if let Some(active) = self.active.as_mut() {
            match active.child.try_wait() {
                Ok(Some(status)) => {
                    let repo = active.repo.clone();
                    let code = status.code();
                    let succeeded = status.success();
                    self.active = None;
                    self.write_status(&repo, code, None);
                    if succeeded {
                        self.scheduler.worker_finished(now);
                    } else {
                        self.scheduler.worker_failed(repo, now);
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    let repo = active.repo.clone();
                    self.active = None;
                    self.write_status(&repo, None, Some(&error.to_string()));
                    self.scheduler.worker_failed(repo, now);
                }
            }
        }
        let Some(repo) = self.scheduler.take_ready(now) else {
            return;
        };
        let base = match self.default_branch(&repo) {
            Ok(base) => base,
            Err(error) => {
                self.write_status(&repo, None, Some(&error));
                self.scheduler.worker_failed(repo, now);
                return;
            }
        };
        match self.spawn(&repo, &base) {
            Ok(child) => self.active = Some(ActiveWake { repo, child }),
            Err(error) => {
                self.write_status(&repo, None, Some(&error.to_string()));
                self.scheduler.worker_failed(repo, now);
            }
        }
    }

    fn spawn(&self, repo: &str, base: &str) -> std::io::Result<ProcessTree> {
        let log_path = self.state_dir.join("daemon").join("steward-wake.log");
        rotate_log(&log_path)?;
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let stderr = stdout.try_clone()?;
        let mut command = Command::new(&self.binary);
        command
            .current_dir(&self.cwd)
            .args(steward_args(
                repo,
                base,
                &self.cwd,
                &self.state_dir,
                &self.global_dir,
                self.mode,
            ))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        ProcessTree::spawn(&mut command)
    }

    fn default_branch(&mut self, repo: &str) -> Result<String, String> {
        if let Some(base) = self.default_branches.get(repo) {
            return Ok(base.clone());
        }
        let base = resolve_default_branch(repo, &self.cwd, &self.global_dir, self.mode)?;
        self.default_branches.insert(repo.to_owned(), base.clone());
        Ok(base)
    }

    fn write_status(&self, repo: &str, exit_code: Option<i32>, error: Option<&str>) {
        let path = self
            .state_dir
            .join("daemon")
            .join("steward-wake-status.json");
        let temp = path.with_extension("json.tmp");
        let payload = json!({
            "schema_version": 1,
            "repo": repo,
            "completed_at": Utc::now().to_rfc3339(),
            "exit_code": exit_code,
            "error": error,
        });
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::write(&temp, format!("{payload}\n")).is_ok() {
            let _ = fs::rename(temp, path);
        }
    }
}

fn steward_args(
    repo: &str,
    base: &str,
    cwd: &Path,
    state_dir: &Path,
    global_dir: &Path,
    mode: RuntimeMode,
) -> Vec<String> {
    vec![
        "--mode".to_owned(),
        mode.as_str().to_owned(),
        "--cwd".to_owned(),
        cwd.display().to_string(),
        "--state-dir".to_owned(),
        state_dir.display().to_string(),
        "--global-dir".to_owned(),
        global_dir.display().to_string(),
        "--json".to_owned(),
        "runner".to_owned(),
        "steward".to_owned(),
        "--repo".to_owned(),
        repo.to_owned(),
        "--base".to_owned(),
        base.to_owned(),
        // The event wake owns routine exact-head queue admission only.
        // Durable retries and capacity preemption remain separate pilots.
        "--max-transient-reruns".to_owned(),
        "0".to_owned(),
        "--no-coalesce".to_owned(),
        "--no-preempt-capacity".to_owned(),
        "--apply".to_owned(),
    ]
}

fn resolve_default_branch(
    repo: &str,
    cwd: &Path,
    global_dir: &Path,
    mode: RuntimeMode,
) -> Result<String, String> {
    let config = LoadedConfig::load_from_cwd_with_global_dir(mode, cwd, global_dir.to_path_buf())
        .map_err(|error| format!("could not load GitHub auth for {repo}: {error}"))?;
    let client = GhClient::from_loaded_config(&config)
        .map_err(|error| format!("could not load GitHub auth for {repo}: {error}"))?
        .with_repo_hint(repo);
    let mut command = client
        .prepare_command_with_auth_timeout(
            cwd,
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
            DEFAULT_BRANCH_TIMEOUT,
        )
        .map_err(|error| format!("could not prepare default-branch query for {repo}: {error}"))?;
    let mut stdout = tempfile::tempfile()
        .map_err(|error| format!("could not capture default branch for {repo}: {error}"))?;
    let mut stderr = tempfile::tempfile()
        .map_err(|error| format!("could not capture default-branch errors for {repo}: {error}"))?;
    command
        .args([
            "repo",
            "view",
            repo,
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone().map_err(|error| {
            format!("could not capture default branch for {repo}: {error}")
        })?))
        .stderr(Stdio::from(stderr.try_clone().map_err(|error| {
            format!("could not capture default-branch errors for {repo}: {error}")
        })?));
    let mut process = ProcessTree::spawn(&mut command)
        .map_err(|error| format!("could not start default-branch query for {repo}: {error}"))?;
    let status = process
        .wait_timeout(DEFAULT_BRANCH_TIMEOUT)
        .map_err(|error| format!("default-branch query failed for {repo}: {error}"))?
        .ok_or_else(|| {
            process.terminate();
            format!(
                "default-branch query timed out for {repo} after {} seconds",
                DEFAULT_BRANCH_TIMEOUT.as_secs()
            )
        })?;
    process.terminate();

    let read_capture = |file: &mut std::fs::File| -> std::io::Result<String> {
        file.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    };
    let output = read_capture(&mut stdout)
        .map_err(|error| format!("could not read default branch for {repo}: {error}"))?;
    let error = read_capture(&mut stderr)
        .map_err(|read_error| format!("could not read GitHub error for {repo}: {read_error}"))?;
    if !status.success() {
        return Err(format!(
            "default-branch query failed for {repo}: {}",
            error.trim()
        ));
    }
    let base = output.trim();
    if base.is_empty() {
        return Err(format!("GitHub returned no default branch for {repo}"));
    }
    Ok(base.to_owned())
}

#[derive(Debug)]
struct StewardWakeScheduler {
    repos: BTreeSet<String>,
    pending: BTreeSet<String>,
    retry_at: BTreeMap<String, Instant>,
    ready_at: Option<Instant>,
    next_reconcile_at: Instant,
}

impl StewardWakeScheduler {
    fn new(repos: &[String], now: Instant) -> Self {
        let repos = repos.iter().map(|repo| canonical_repo(repo)).collect();
        Self {
            repos,
            pending: BTreeSet::new(),
            retry_at: BTreeMap::new(),
            ready_at: None,
            // A daemon start/rejoin always gets one authoritative catch-up.
            next_reconcile_at: now,
        }
    }

    fn observe(&mut self, event: &Value, now: Instant) {
        let Some(repo) = wake_repo(event).map(canonical_repo) else {
            return;
        };
        if self.repos.contains(&repo) {
            self.retry_at.remove(&repo);
            self.pending.insert(repo);
            self.ready_at.get_or_insert(now + EVENT_DEBOUNCE);
        }
    }

    fn schedule_periodic(&mut self, now: Instant) {
        if now < self.next_reconcile_at {
            return;
        }
        self.pending.extend(self.repos.iter().cloned());
        self.retry_at.clear();
        self.ready_at.get_or_insert(now);
        self.next_reconcile_at = now + RECONCILE_INTERVAL;
    }

    fn take_ready(&mut self, now: Instant) -> Option<String> {
        let retries = self
            .retry_at
            .iter()
            .filter_map(|(repo, retry_at)| (*retry_at <= now).then_some(repo.clone()))
            .collect::<Vec<_>>();
        for repo in retries {
            self.retry_at.remove(&repo);
            self.pending.insert(repo);
            self.ready_at = Some(self.ready_at.map_or(now, |ready_at| ready_at.min(now)));
        }
        if self.ready_at.is_none_or(|ready_at| now < ready_at) {
            return None;
        }
        let repo = self.pending.pop_first();
        if let Some(repo) = &repo {
            self.retry_at.remove(repo);
        }
        if self.pending.is_empty() {
            self.ready_at = None;
        }
        repo
    }

    fn worker_finished(&mut self, now: Instant) {
        if !self.pending.is_empty() {
            self.ready_at = Some(now);
        }
    }

    fn worker_failed(&mut self, repo: String, now: Instant) {
        self.retry_at.insert(repo, now + FAILURE_RETRY_DELAY);
        self.ready_at = (!self.pending.is_empty()).then_some(now);
    }
}

fn wake_repo(event: &Value) -> Option<&str> {
    let kind = event.get("kind")?.as_str()?;
    let payload = event.get("payload")?;
    let repo = payload.get("repo")?.as_str()?;
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let relevant = match kind {
        "workflow_run" | "check_suite" => action == "completed" || status == "completed",
        "pull_request" => matches!(
            action,
            "opened" | "reopened" | "ready_for_review" | "synchronize" | "labeled" | "unlabeled"
        ),
        _ => false,
    };
    relevant.then_some(repo)
}

fn canonical_repo(repo: &str) -> String {
    repo.trim().trim_end_matches(".git").to_ascii_lowercase()
}

fn rotate_log(path: &Path) -> std::io::Result<()> {
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES) {
        let previous = path.with_extension("log.previous");
        let _ = fs::remove_file(&previous);
        fs::rename(path, previous)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::identity::RuntimeMode;

    use super::{
        EVENT_DEBOUNCE, RECONCILE_INTERVAL, StewardWakeScheduler, steward_args, wake_repo,
    };

    #[test]
    fn terminal_ci_and_pr_transitions_wake_but_noisy_updates_do_not() {
        let workflow = json!({"kind":"workflow_run","payload":{"repo":"Owner/Repo","action":"completed","status":"completed"}});
        let suite = json!({"kind":"check_suite","payload":{"repo":"Owner/Repo","action":"completed","status":"completed"}});
        let labeled =
            json!({"kind":"pull_request","payload":{"repo":"Owner/Repo","action":"labeled"}});
        let in_progress = json!({"kind":"workflow_run","payload":{"repo":"Owner/Repo","action":"in_progress","status":"in_progress"}});
        let check_run = json!({"kind":"check_run","payload":{"repo":"Owner/Repo","action":"completed","status":"completed"}});
        assert_eq!(wake_repo(&workflow), Some("Owner/Repo"));
        assert_eq!(wake_repo(&suite), Some("Owner/Repo"));
        assert_eq!(wake_repo(&labeled), Some("Owner/Repo"));
        assert_eq!(wake_repo(&in_progress), None);
        assert_eq!(wake_repo(&check_run), None);
    }

    #[test]
    fn events_are_repo_scoped_debounced_and_coalesced() {
        let now = Instant::now();
        let mut scheduler = StewardWakeScheduler::new(&["Owner/Repo".to_owned()], now);
        // Consume the startup catch-up first.
        scheduler.schedule_periodic(now);
        assert_eq!(scheduler.take_ready(now).as_deref(), Some("owner/repo"));
        let event =
            json!({"kind":"workflow_run","payload":{"repo":"OWNER/REPO","action":"completed"}});
        scheduler.observe(&event, now);
        scheduler.observe(&event, now + Duration::from_secs(1));
        let just_before_ready = (now + EVENT_DEBOUNCE)
            .checked_sub(Duration::from_millis(1))
            .expect("debounce exceeds one millisecond");
        assert_eq!(scheduler.take_ready(just_before_ready), None);
        assert_eq!(
            scheduler.take_ready(now + EVENT_DEBOUNCE).as_deref(),
            Some("owner/repo")
        );
        assert_eq!(scheduler.take_ready(now + EVENT_DEBOUNCE), None);
    }

    #[test]
    fn unrelated_repos_never_enter_the_authority_queue() {
        let now = Instant::now();
        let mut scheduler = StewardWakeScheduler::new(&["owner/pulp".to_owned()], now);
        scheduler.next_reconcile_at = now + RECONCILE_INTERVAL;
        scheduler.observe(
            &json!({"kind":"check_suite","payload":{"repo":"owner/forge","action":"completed"}}),
            now,
        );
        assert_eq!(scheduler.take_ready(now + EVENT_DEBOUNCE), None);
    }

    #[test]
    fn periodic_reconcile_catches_events_missed_while_offline() {
        let now = Instant::now();
        let mut scheduler =
            StewardWakeScheduler::new(&["owner/pulp".to_owned(), "owner/forge".to_owned()], now);
        scheduler.schedule_periodic(now);
        assert_eq!(scheduler.take_ready(now).as_deref(), Some("owner/forge"));
        scheduler.worker_finished(now);
        assert_eq!(scheduler.take_ready(now).as_deref(), Some("owner/pulp"));
        let just_before_reconcile = (now + RECONCILE_INTERVAL)
            .checked_sub(Duration::from_secs(1))
            .expect("reconcile interval exceeds one second");
        scheduler.schedule_periodic(just_before_reconcile);
        assert_eq!(scheduler.take_ready(now), None);
        scheduler.schedule_periodic(now + RECONCILE_INTERVAL);
        assert!(scheduler.take_ready(now + RECONCILE_INTERVAL).is_some());
    }

    #[test]
    fn failed_worker_is_retried_after_a_bounded_delay() {
        let now = Instant::now();
        let mut scheduler = StewardWakeScheduler::new(&["owner/repo".to_owned()], now);
        scheduler.schedule_periodic(now);
        assert_eq!(scheduler.take_ready(now).as_deref(), Some("owner/repo"));
        scheduler.worker_failed("owner/repo".to_owned(), now);
        let just_before_retry = (now + super::FAILURE_RETRY_DELAY)
            .checked_sub(Duration::from_millis(1))
            .expect("retry delay exceeds one millisecond");
        assert_eq!(scheduler.take_ready(just_before_retry), None);
        assert_eq!(
            scheduler
                .take_ready(now + super::FAILURE_RETRY_DELAY)
                .as_deref(),
            Some("owner/repo")
        );
    }

    #[test]
    fn failed_repo_does_not_starve_an_already_ready_peer() {
        let now = Instant::now();
        let mut scheduler =
            StewardWakeScheduler::new(&["owner/a".to_owned(), "owner/b".to_owned()], now);
        scheduler.schedule_periodic(now);
        assert_eq!(scheduler.take_ready(now).as_deref(), Some("owner/a"));
        scheduler.worker_failed("owner/a".to_owned(), now);
        assert_eq!(scheduler.take_ready(now).as_deref(), Some("owner/b"));
        scheduler.worker_finished(now);
        assert_eq!(scheduler.take_ready(now), None);
        assert_eq!(
            scheduler
                .take_ready(now + super::FAILURE_RETRY_DELAY)
                .as_deref(),
            Some("owner/a")
        );
    }

    #[test]
    fn immediate_follow_up_consumes_stale_retry_for_same_repo() {
        let now = Instant::now();
        let mut scheduler = StewardWakeScheduler::new(&["owner/repo".to_owned()], now);
        scheduler.schedule_periodic(now);
        assert_eq!(scheduler.take_ready(now).as_deref(), Some("owner/repo"));
        scheduler.pending.insert("owner/repo".to_owned());
        scheduler.worker_failed("owner/repo".to_owned(), now);
        assert_eq!(scheduler.take_ready(now).as_deref(), Some("owner/repo"));
        scheduler.worker_finished(now);
        assert_eq!(scheduler.take_ready(now + super::FAILURE_RETRY_DELAY), None);
    }

    #[test]
    fn worker_is_apply_but_excludes_retry_cleanup_and_preemption_pilots() {
        let args = steward_args(
            "owner/repo",
            "trunk",
            std::path::Path::new("/repo"),
            std::path::Path::new("/state"),
            std::path::Path::new("/global"),
            RuntimeMode::Shipyard,
        );
        assert!(args.windows(2).any(|pair| pair == ["--repo", "owner/repo"]));
        assert!(args.windows(2).any(|pair| pair == ["--base", "trunk"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--max-transient-reruns", "0"])
        );
        assert!(args.contains(&"--no-coalesce".to_owned()));
        assert!(args.contains(&"--no-preempt-capacity".to_owned()));
        assert!(args.contains(&"--apply".to_owned()));
    }
}
