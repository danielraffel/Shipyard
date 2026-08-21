//! Event-driven, single-authority merge-steward wakeups.
//!
//! The daemon already receives signed repository webhooks. This module turns
//! terminal CI/PR transitions into coalesced steward passes on the one machine
//! selected by `merge_queue.mutation_machine`. A low-frequency reconcile is a
//! safety net for events missed while the authority host was offline.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Value, json};

use crate::identity::RuntimeMode;
use crate::merge_queue_control::authority_status;
use crate::paths::RuntimePaths;

const EVENT_DEBOUNCE: Duration = Duration::from_secs(2);
const RECONCILE_INTERVAL: Duration = Duration::from_mins(30);
const FAILURE_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_LOG_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
struct ActiveWake {
    repo: String,
    child: Child,
}

/// Live event-to-steward bridge owned by the daemon.
#[derive(Debug)]
pub struct StewardWakeRuntime {
    scheduler: StewardWakeScheduler,
    active: Option<ActiveWake>,
    binary: PathBuf,
    cwd: PathBuf,
    state_dir: PathBuf,
    global_dir: PathBuf,
    mode: RuntimeMode,
}

impl Drop for StewardWakeRuntime {
    fn drop(&mut self) {
        if let Some(active) = self.active.as_mut() {
            let _ = active.child.kill();
            let _ = active.child.wait();
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
        cwd: &Path,
        mode: RuntimeMode,
    ) -> Option<Self> {
        if cfg!(test) || mode != RuntimeMode::Shipyard || repos.is_empty() {
            return None;
        }
        let global_dir = RuntimePaths::current(mode).global_dir;
        let authority = authority_status(state_dir, cwd, mode, &global_dir).ok()?;
        if authority.get("authority_matches").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let binary = std::env::current_exe().ok()?;
        Some(Self {
            scheduler: StewardWakeScheduler::new(repos, Instant::now()),
            active: None,
            binary,
            cwd: cwd.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            global_dir,
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
                    self.active = None;
                    self.write_status(&repo, code, None);
                    self.scheduler.worker_finished(now);
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
        match self.spawn(&repo) {
            Ok(child) => self.active = Some(ActiveWake { repo, child }),
            Err(error) => {
                self.write_status(&repo, None, Some(&error.to_string()));
                self.scheduler.worker_failed(repo, now);
            }
        }
    }

    fn spawn(&self, repo: &str) -> std::io::Result<Child> {
        let log_path = self.state_dir.join("daemon").join("steward-wake.log");
        rotate_log(&log_path)?;
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let stderr = stdout.try_clone()?;
        Command::new(&self.binary)
            .current_dir(&self.cwd)
            .args(steward_args(
                repo,
                &self.cwd,
                &self.state_dir,
                &self.global_dir,
                self.mode,
            ))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
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
        "main".to_owned(),
        // The event wake owns routine exact-head queue admission only.
        // Durable retries and capacity preemption remain separate pilots.
        "--max-transient-reruns".to_owned(),
        "0".to_owned(),
        "--no-coalesce".to_owned(),
        "--no-preempt-capacity".to_owned(),
        "--apply".to_owned(),
    ]
}

#[derive(Debug)]
struct StewardWakeScheduler {
    repos: BTreeSet<String>,
    pending: BTreeSet<String>,
    ready_at: Option<Instant>,
    next_reconcile_at: Instant,
}

impl StewardWakeScheduler {
    fn new(repos: &[String], now: Instant) -> Self {
        let repos = repos.iter().map(|repo| canonical_repo(repo)).collect();
        Self {
            repos,
            pending: BTreeSet::new(),
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
            self.pending.insert(repo);
            self.ready_at.get_or_insert(now + EVENT_DEBOUNCE);
        }
    }

    fn schedule_periodic(&mut self, now: Instant) {
        if now < self.next_reconcile_at {
            return;
        }
        self.pending.extend(self.repos.iter().cloned());
        self.ready_at.get_or_insert(now);
        self.next_reconcile_at = now + RECONCILE_INTERVAL;
    }

    fn take_ready(&mut self, now: Instant) -> Option<String> {
        if self.ready_at.is_none_or(|ready_at| now < ready_at) {
            return None;
        }
        let repo = self.pending.pop_first();
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
        self.pending.insert(repo);
        self.ready_at = Some(now + FAILURE_RETRY_DELAY);
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
    fn worker_is_apply_but_excludes_retry_cleanup_and_preemption_pilots() {
        let args = steward_args(
            "owner/repo",
            std::path::Path::new("/repo"),
            std::path::Path::new("/state"),
            std::path::Path::new("/global"),
            RuntimeMode::Shipyard,
        );
        assert!(args.windows(2).any(|pair| pair == ["--repo", "owner/repo"]));
        assert!(args.windows(2).any(|pair| pair == ["--base", "main"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--max-transient-reruns", "0"])
        );
        assert!(args.contains(&"--no-coalesce".to_owned()));
        assert!(args.contains(&"--no-preempt-capacity".to_owned()));
        assert!(args.contains(&"--apply".to_owned()));
    }
}
