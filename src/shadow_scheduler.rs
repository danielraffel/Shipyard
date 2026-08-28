//! Subscriber-independent, read-only PR observation from the canonical ledger.
//!
//! This phase intentionally has no activation, dispatch, outbox, GitHub
//! mutation, or model boundary. Webhooks only accelerate an exact observation;
//! a bounded round-robin catch-up heals missed deliveries.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::LoadedConfig;
use crate::gh::GhClient;
use crate::identity::RuntimeMode;
use crate::reconcile::{
    ProvenancedFetchError, ReconcileFetchError,
    fetch_head_and_provenanced_status_check_rollup_for_repo_with_client,
};
use crate::work_ledger::ShadowPrTarget;
use crate::work_ledger::WorkLedger;

mod health;

pub use health::ShadowObserverHealth;
use health::{load as load_observer_health, save as save_observer_health};

/// Delay used to coalesce a burst of webhook events for the same PR.
pub const SHADOW_WEBHOOK_DEBOUNCE: Duration = Duration::from_secs(2);
/// Maximum age of a webhook burst before it must run despite continued traffic.
pub const SHADOW_WEBHOOK_MAX_COALESCE: Duration = Duration::from_secs(10);
/// Missed-event healing cadence. Each pass is separately request-budgeted.
pub const SHADOW_CATCH_UP_INTERVAL: Duration = Duration::from_mins(5);
/// Maximum GitHub snapshots fetched by one daemon catch-up pass.
pub const SHADOW_CATCH_UP_BUDGET: usize = 8;
/// Maximum exact targets fetched after one coalesced webhook burst.
pub const SHADOW_WEBHOOK_BUDGET: usize = 16;
/// Maximum time spent resolving one target repository's configured auth.
pub const SHADOW_AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// Complete wall-clock deadline shared by every target in one shadow pass.
pub const SHADOW_PASS_TIMEOUT: Duration = Duration::from_mins(1);
/// Maximum simultaneous GitHub reads in the shadow lane.
pub const SHADOW_FETCH_CONCURRENCY: usize = 4;
/// Minimum interval before the same exact target is webhook-observed again.
pub const SHADOW_TARGET_COOLDOWN: Duration = Duration::from_secs(30);
/// Hard rolling-hour GitHub request ceiling for this passive lane.
pub const SHADOW_HOURLY_API_CEILING: usize = 240;
/// Maximum provenanced GraphQL pages one exact target may consume.
pub const SHADOW_MAX_REQUESTS_PER_TARGET: usize = 10;
/// Maximum pending webhook scopes retained before periodic catch-up takes over.
pub const SHADOW_PENDING_SCOPE_LIMIT: usize = 1_024;
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ShadowBudgetEntry {
    epoch_seconds: u64,
    api_requests: usize,
}

/// Why one bounded observation pass was scheduled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowTrigger {
    /// A webhook delivery accelerated exact matching work.
    Webhook,
    /// The bounded periodic sweep healed potentially missed deliveries.
    PeriodicCatchUp,
}

/// Non-secret webhook scope used only to select ledger targets.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ShadowWebhookScope {
    repo: String,
    pr: Option<u64>,
    head_sha: Option<String>,
}

/// One normalized, read-only GitHub snapshot for an exact ledger head.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ShadowObservation {
    /// Canonical repository.
    pub repo: String,
    /// Pull-request number.
    pub pr: u64,
    /// Immutable ledger head being checked.
    pub expected_head_sha: String,
    /// Number of nonterminal ledger records collapsed into this exact target.
    pub work_items: u64,
    /// Live GitHub head returned with the check rollup.
    pub observed_head_sha: String,
    /// Whether the live result still describes the ledger head.
    pub exact_head: bool,
    /// Digest of the normalized check snapshot and exact-head fence.
    pub snapshot_digest: String,
    /// Digest of canonical ledger identity and repository policy only.
    pub ledger_digest: String,
    /// Digest of the live GitHub head and normalized producer-provenanced checks.
    pub github_digest: String,
    /// Number of checks currently queued or running.
    pub pending_checks: u64,
    /// Number of checks with a successful terminal conclusion.
    pub passed_checks: u64,
    /// Number of checks with a non-success terminal conclusion.
    pub failed_checks: u64,
    /// Policy revision read from the canonical ledger.
    pub policy_revision: u64,
    /// Primary platform. Pulp, Forge, and Vellum currently configure `macos`.
    pub primary_platform: String,
    /// Compatibility policy; currently `independent` for the three projects.
    pub compatibility_mode: String,
    /// Rule that alone permits cross-lane blocking in a later active phase.
    pub blocking_rule: String,
}

/// Transition kind emitted by the read-only observer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowObservationTransitionKind {
    /// A previously observed exact snapshot changed.
    SnapshotChanged,
    /// One exact target changed from observable or unknown to fetch-failed.
    FetchFailed,
    /// A previously failed target became observable again.
    FetchRecovered,
}

/// A changed observation state. Baselines and unchanged polls are suppressed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ShadowObservationTransition {
    /// Transition classification.
    pub kind: ShadowObservationTransitionKind,
    /// Canonical repository.
    pub repo: String,
    /// Pull-request number.
    pub pr: u64,
    /// Immutable ledger head being observed.
    pub expected_head_sha: String,
    /// Exact repository policy revision associated with this transition.
    pub policy_revision: u64,
    /// Current observation for snapshot changes and recovery.
    pub observation: Option<ShadowObservation>,
    /// Digest from the preceding successful observation, when available.
    pub previous_snapshot_digest: Option<String>,
    /// Stable failure class for a fetch-failed transition.
    pub failure_class: Option<String>,
}

/// One failed read boundary, redacted to exact target and stable class.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ShadowFetchFailure {
    /// Canonical repository.
    pub repo: String,
    /// Pull-request number.
    pub pr: u64,
    /// Immutable ledger head being observed.
    pub expected_head_sha: String,
    /// Exact repository policy revision at the failed observation boundary.
    pub policy_revision: u64,
    /// Stable error class; command output is intentionally excluded.
    pub failure_class: String,
}

/// Transition plus the bounded pass evidence that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowTransitionEvidence {
    /// Changed exact-head snapshot.
    pub transition: ShadowObservationTransition,
    /// Scheduling cause.
    pub trigger: ShadowTrigger,
    /// GitHub read requests spent by the complete bounded pass.
    pub api_requests: usize,
    /// Failed requests in the complete bounded pass.
    pub fetch_errors: usize,
    /// Complete pass wall-clock latency.
    pub elapsed_ms: u64,
}

/// Evidence from one bounded, zero-model observation pass.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ShadowObservationReport {
    /// Scheduling cause.
    pub trigger: ShadowTrigger,
    /// Exact targets selected under the trigger's request budget.
    pub selected_targets: usize,
    /// GitHub read requests attempted, including bounded pagination pages.
    pub api_requests: usize,
    /// Successful exact snapshots.
    pub observations: Vec<ShadowObservation>,
    /// Failed exact reads, without stderr or credential-bearing detail.
    pub failures: Vec<ShadowFetchFailure>,
    /// Failed read requests. No retry occurs inside the same pass.
    pub fetch_errors: usize,
    /// Stable lane-level failure class when the pass could not perform reads.
    pub observer_failure_class: Option<String>,
    /// Wall-clock cost of the bounded pass.
    pub elapsed_ms: u64,
    /// Activation remains impossible in this phase.
    pub activation_enabled: bool,
    /// Dispatch and wake delivery remain impossible in this phase.
    pub dispatch_enabled: bool,
    /// Routine observation never invokes a model.
    pub model_calls: u64,
}

/// In-memory timing, debounce, and transition state for the shadow daemon lane.
#[derive(Debug)]
pub struct ShadowScheduler {
    next_catch_up_at: Instant,
    webhook_due_at: Option<Instant>,
    webhook_first_at: Option<Instant>,
    pending_webhooks: BTreeSet<ShadowWebhookScope>,
    periodic_cursor: usize,
    in_flight: bool,
    snapshots: BTreeMap<(String, u64, String), String>,
    failed_targets: BTreeMap<(String, u64, String), (String, u64)>,
    target_cooldowns: BTreeMap<(String, u64, String), Instant>,
    api_window: VecDeque<(Instant, usize)>,
}

/// Complete daemon-side ownership of the read-only shadow lane.
///
/// Keeping ledger enumeration, debounce state, worker lifetime, and
/// transition suppression here prevents the already-busy daemon loop from
/// acquiring feature-specific state-machine branches.
#[derive(Debug)]
pub struct ShadowDaemonLane {
    mode: RuntimeMode,
    global_dir: PathBuf,
    state_dir: PathBuf,
    cwd: PathBuf,
    scheduler: ShadowScheduler,
    sender: mpsc::Sender<ShadowObservationReport>,
    receiver: mpsc::Receiver<ShadowObservationReport>,
    ledger_error: Option<String>,
    next_ledger_retry_at: Option<Instant>,
    budget_path: PathBuf,
    budget_entries: Vec<ShadowBudgetEntry>,
    reserved_requests: Option<usize>,
    health_path: PathBuf,
    health: Arc<Mutex<ShadowObserverHealth>>,
}

impl ShadowDaemonLane {
    /// Construct a lane whose first bounded catch-up is immediately due.
    #[must_use]
    pub fn new(
        mode: RuntimeMode,
        global_dir: PathBuf,
        state_dir: PathBuf,
        cwd: PathBuf,
        now: Instant,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let budget_path = state_dir.join("shadow-observer-budget.json");
        let health_path = state_dir.join("shadow-observer-health.json");
        let (budget_entries, restored_requests) = match load_request_budget(&budget_path) {
            Ok(entries) => {
                let requests = entries.iter().map(|entry| entry.api_requests).sum();
                (entries, requests)
            }
            Err(error) => {
                eprintln!("shipyard daemon: shadow request budget unavailable: {error}");
                (Vec::new(), SHADOW_HOURLY_API_CEILING)
            }
        };
        let mut health = load_observer_health(&health_path).unwrap_or_else(|_| {
            let mut health = ShadowObserverHealth::default();
            health.last_failure_at = Some(epoch_seconds());
            health.last_failure_class = Some("health_state_unavailable".to_owned());
            health
        });
        health.normalize_schema();
        if health.in_flight_since.take().is_some() {
            health.last_failure_at = Some(epoch_seconds());
            health.last_failure_class = Some("daemon_restarted_during_pass".to_owned());
        }
        health.reserved_requests = 0;
        health.rolling_hour_requests = restored_requests;
        let mut scheduler = ShadowScheduler::new(now);
        scheduler.periodic_cursor = health.periodic_cursor;
        let wall_now = epoch_seconds();
        for entry in &budget_entries {
            let age = Duration::from_secs(wall_now.saturating_sub(entry.epoch_seconds));
            let charged_at = now.checked_sub(age).unwrap_or(now);
            scheduler
                .api_window
                .push_back((charged_at, entry.api_requests));
        }
        let health = Arc::new(Mutex::new(health));
        let mut lane = Self {
            mode,
            global_dir,
            state_dir,
            cwd,
            scheduler,
            sender,
            receiver,
            ledger_error: None,
            next_ledger_retry_at: None,
            budget_path,
            budget_entries,
            reserved_requests: None,
            health_path,
            health,
        };
        lane.refresh_schedule_health(now);
        lane
    }

    /// Return the short-held in-memory snapshot used by daemon status.
    #[must_use]
    pub fn health_handle(&self) -> Arc<Mutex<ShadowObserverHealth>> {
        Arc::clone(&self.health)
    }

    /// Coalesce a relevant webhook independently of daemon IPC subscribers.
    pub fn note_webhook(&mut self, event: &Value, now: Instant) {
        if self.scheduler.note_webhook(event, now) {
            self.refresh_schedule_health(now);
        }
    }

    /// Drain completed evidence and start at most one due bounded pass.
    pub fn tick(&mut self, now: Instant) -> Vec<ShadowTransitionEvidence> {
        self.refresh_budget_health(now);
        let mut evidence = Vec::new();
        while let Ok(report) = self.receiver.try_recv() {
            evidence.extend(self.finish_report(&report, now));
        }
        let Some(trigger) = self.scheduler.due_trigger(now) else {
            return evidence;
        };
        if self.next_ledger_retry_at.is_some_and(|retry| now < retry) {
            self.refresh_schedule_health(now);
            return evidence;
        }
        let Some(targets) = self.targets(now) else {
            self.refresh_schedule_health(now);
            return evidence;
        };
        self.update_health(|health| health.exact_target_count = targets.len());
        self.scheduler.retain_targets(&targets);
        let selected = self.scheduler.begin_pass(trigger, &targets, now);
        let selected_count = selected.len();
        let cursor = self.scheduler.periodic_cursor;
        let wall_now = epoch_seconds();
        self.update_health(|health| {
            health.periodic_cursor = cursor;
            health.in_flight_since = Some(wall_now);
            health.last_trigger = Some(trigger);
            health.last_selected_targets = selected_count;
        });
        self.refresh_schedule_health(now);
        if selected.is_empty() {
            let report = empty_report(trigger);
            evidence.extend(self.finish_report(&report, now));
            return evidence;
        }
        let reservation = selected.len() * SHADOW_MAX_REQUESTS_PER_TARGET;
        if let Err(error) = self.reserve_requests(reservation, now) {
            eprintln!("shipyard daemon: shadow request reservation failed: {error}");
            self.scheduler
                .api_window
                .push_back((now, SHADOW_HOURLY_API_CEILING));
            let mut report = empty_report(trigger);
            report.observer_failure_class = Some("budget_reservation".to_owned());
            evidence.extend(self.finish_report(&report, now));
            return evidence;
        }
        let mode = self.mode;
        let global_dir = self.global_dir.clone();
        let cwd = self.cwd.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let report = std::panic::catch_unwind(|| {
                observe_targets(mode, &global_dir, &cwd, trigger, &selected)
            })
            .unwrap_or_else(|_| worker_panic_report(trigger, &selected));
            let _ = sender.send(report);
        });
        evidence
    }

    fn targets(&mut self, now: Instant) -> Option<Vec<ShadowPrTarget>> {
        match WorkLedger::open_existing(&self.state_dir) {
            Ok(Some(ledger)) => match ledger.shadow_pr_targets() {
                Ok(targets) => {
                    self.log_recovery();
                    self.next_ledger_retry_at = None;
                    Some(targets)
                }
                Err(error) => {
                    self.log_ledger_error(&error);
                    self.next_ledger_retry_at = Some(now + Duration::from_secs(5));
                    None
                }
            },
            Ok(None) => {
                self.ledger_error = None;
                self.next_ledger_retry_at = None;
                Some(Vec::new())
            }
            Err(error) => {
                self.log_ledger_error(&error);
                self.next_ledger_retry_at = Some(now + Duration::from_secs(5));
                None
            }
        }
    }

    fn finish_report(
        &mut self,
        report: &ShadowObservationReport,
        now: Instant,
    ) -> Vec<ShadowTransitionEvidence> {
        let reserved = self.reserved_requests.take().unwrap_or(0);
        let had_reservation = reserved > 0;
        if had_reservation {
            self.budget_entries.pop();
            self.scheduler.api_window.pop_back();
        }
        if report.api_requests > 0 {
            self.budget_entries.push(ShadowBudgetEntry {
                epoch_seconds: epoch_seconds(),
                api_requests: report.api_requests,
            });
        }
        let mut budget_persistence_failed = false;
        if had_reservation || report.api_requests > 0 {
            prune_budget_entries(&mut self.budget_entries, epoch_seconds());
            if let Err(error) = save_request_budget(&self.budget_path, &self.budget_entries) {
                eprintln!("shipyard daemon: shadow request budget persistence failed: {error}");
                self.scheduler
                    .api_window
                    .push_back((now, SHADOW_HOURLY_API_CEILING));
                budget_persistence_failed = true;
            }
        }
        let rolling_hour_requests =
            reported_rolling_requests(&self.budget_entries, budget_persistence_failed)
                .max(self.scheduler.current_request_usage());
        let completed_at = epoch_seconds();
        let failure_class = report
            .observer_failure_class
            .clone()
            .or_else(|| budget_persistence_failed.then(|| "budget_persistence".to_owned()))
            .or_else(|| {
                report
                    .failures
                    .first()
                    .map(|failure| failure.failure_class.clone())
            });
        self.update_health(|health| {
            health.in_flight_since = None;
            health.reserved_requests = 0;
            health.last_reserved_requests = reserved;
            health.last_actual_requests = report.api_requests;
            health.rolling_hour_requests = rolling_hour_requests;
            if report.fetch_errors == 0 && failure_class.is_none() {
                health.last_success_at = Some(completed_at);
            } else {
                health.last_failure_at = Some(completed_at);
                health.last_failure_class =
                    failure_class.or_else(|| Some("fetch_failed".to_owned()));
            }
        });
        let transitions = self.scheduler.finish_pass_at(report, now);
        self.refresh_schedule_health(now);
        transitions
            .into_iter()
            .map(|transition| ShadowTransitionEvidence {
                transition,
                trigger: report.trigger,
                api_requests: report.api_requests,
                fetch_errors: report.fetch_errors,
                elapsed_ms: report.elapsed_ms,
            })
            .collect()
    }

    fn reserve_requests(&mut self, requests: usize, now: Instant) -> Result<(), String> {
        self.budget_entries.push(ShadowBudgetEntry {
            epoch_seconds: epoch_seconds(),
            api_requests: requests,
        });
        if let Err(error) = save_request_budget(&self.budget_path, &self.budget_entries) {
            self.budget_entries.pop();
            return Err(error);
        }
        self.reserved_requests = Some(requests);
        self.scheduler.api_window.push_back((now, requests));
        let rolling_hour_requests = self
            .budget_entries
            .iter()
            .map(|entry| entry.api_requests)
            .sum();
        self.update_health(|health| {
            health.reserved_requests = requests;
            health.rolling_hour_requests = rolling_hour_requests;
        });
        self.refresh_schedule_health(now);
        Ok(())
    }

    fn log_ledger_error(&mut self, error: &crate::work_ledger::WorkLedgerError) {
        let message = error.to_string();
        if self.ledger_error.as_deref() != Some(&message) {
            eprintln!("shipyard daemon: shadow ledger observation unavailable: {message}");
            self.ledger_error = Some(message);
            let failed_at = epoch_seconds();
            self.update_health(|health| {
                health.last_failure_at = Some(failed_at);
                health.last_failure_class = Some("ledger".to_owned());
            });
        }
    }

    fn log_recovery(&mut self) {
        if self.ledger_error.take().is_some() {
            eprintln!("shipyard daemon: shadow ledger observation recovered");
        }
    }

    fn refresh_budget_health(&mut self, now: Instant) {
        self.scheduler.prune_request_budget(now);
        let before = self.budget_entries.len();
        prune_budget_entries(&mut self.budget_entries, epoch_seconds());
        let persistence_failed = if self.budget_entries.len() == before {
            false
        } else if let Err(error) = save_request_budget(&self.budget_path, &self.budget_entries) {
            eprintln!("shipyard daemon: shadow request budget persistence failed: {error}");
            true
        } else {
            false
        };
        let rolling_hour_requests =
            reported_rolling_requests(&self.budget_entries, persistence_failed)
                .max(self.scheduler.current_request_usage());
        let failed_at = epoch_seconds();
        let health_changed = self.health.lock().is_ok_and(|health| {
            health.rolling_hour_requests != rolling_hour_requests || persistence_failed
        });
        if !health_changed {
            return;
        }
        self.update_health(|health| {
            health.rolling_hour_requests = rolling_hour_requests;
            if persistence_failed {
                health.last_failure_at = Some(failed_at);
                health.last_failure_class = Some("budget_persistence".to_owned());
            }
        });
    }

    fn refresh_schedule_health(&mut self, now: Instant) {
        let retry_after = self
            .next_ledger_retry_at
            .map_or(Duration::ZERO, |retry| retry.saturating_duration_since(now));
        let next_due = self
            .scheduler
            .next_due_after(now)
            .max(retry_after)
            .as_secs();
        let next_due_at = epoch_seconds().saturating_add(next_due);
        self.update_health(|health| health.next_due_at = Some(next_due_at));
    }

    fn update_health(&mut self, update: impl FnOnce(&mut ShadowObserverHealth)) {
        if let Ok(mut health) = self.health.lock() {
            update(&mut health);
        }
        self.persist_health();
    }

    fn persist_health(&mut self) {
        let snapshot = self.health.lock().ok().map(|health| health.clone());
        let Some(snapshot) = snapshot else {
            return;
        };
        if let Err(error) = save_observer_health(&self.health_path, &snapshot) {
            eprintln!("shipyard daemon: shadow health persistence failed: {error}");
            if let Ok(mut health) = self.health.lock() {
                health.last_failure_at = Some(epoch_seconds());
                health.last_failure_class = Some("health_persistence".to_owned());
            }
        }
    }
}

impl ShadowScheduler {
    /// Start with an immediately due bounded catch-up.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            next_catch_up_at: now,
            webhook_due_at: None,
            webhook_first_at: None,
            pending_webhooks: BTreeSet::new(),
            periodic_cursor: 0,
            in_flight: false,
            snapshots: BTreeMap::new(),
            failed_targets: BTreeMap::new(),
            target_cooldowns: BTreeMap::new(),
            api_window: VecDeque::new(),
        }
    }

    /// Coalesce a relevant webhook without requiring an IPC subscriber.
    pub fn note_webhook(&mut self, event: &Value, now: Instant) -> bool {
        let Some(scope) = webhook_scope(event) else {
            return false;
        };
        self.pending_webhooks.insert(scope);
        while self.pending_webhooks.len() > SHADOW_PENDING_SCOPE_LIMIT {
            self.pending_webhooks.pop_first();
        }
        let first = *self.webhook_first_at.get_or_insert(now);
        self.webhook_due_at =
            Some((now + SHADOW_WEBHOOK_DEBOUNCE).min(first + SHADOW_WEBHOOK_MAX_COALESCE));
        true
    }

    /// Select a due trigger. A running pass is never duplicated.
    #[must_use]
    pub fn due_trigger(&mut self, now: Instant) -> Option<ShadowTrigger> {
        self.prune_request_budget(now);
        if self.in_flight {
            return None;
        }
        if self.remaining_request_budget() < SHADOW_MAX_REQUESTS_PER_TARGET {
            return None;
        }
        if now >= self.next_catch_up_at {
            return Some(ShadowTrigger::PeriodicCatchUp);
        }
        self.webhook_due_at
            .is_some_and(|due| now >= due)
            .then_some(ShadowTrigger::Webhook)
    }

    fn next_due_after(&self, now: Instant) -> Duration {
        let scheduled = std::iter::once(self.next_catch_up_at)
            .chain(self.webhook_due_at)
            .map(|due| due.saturating_duration_since(now))
            .min()
            .unwrap_or(Duration::ZERO);
        scheduled.max(self.request_budget_available_after(now))
    }

    fn request_budget_available_after(&self, now: Instant) -> Duration {
        let mut used = self
            .api_window
            .iter()
            .map(|(_, count)| count)
            .sum::<usize>();
        if used <= SHADOW_HOURLY_API_CEILING - SHADOW_MAX_REQUESTS_PER_TARGET {
            return Duration::ZERO;
        }
        for (charged_at, count) in &self.api_window {
            used = used.saturating_sub(*count);
            if used <= SHADOW_HOURLY_API_CEILING - SHADOW_MAX_REQUESTS_PER_TARGET {
                return (*charged_at + Duration::from_hours(1)).saturating_duration_since(now);
            }
        }
        Duration::from_hours(1)
    }

    fn prune_request_budget(&mut self, now: Instant) {
        while self
            .api_window
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= Duration::from_hours(1))
        {
            self.api_window.pop_front();
        }
    }

    fn current_request_usage(&self) -> usize {
        self.api_window.iter().map(|(_, count)| count).sum()
    }

    /// Select bounded exact targets and fence one pass as in flight.
    pub fn begin_pass(
        &mut self,
        trigger: ShadowTrigger,
        targets: &[ShadowPrTarget],
        now: Instant,
    ) -> Vec<ShadowPrTarget> {
        debug_assert!(!self.in_flight, "one shadow pass at a time");
        self.in_flight = true;
        let request_bounded_targets =
            self.remaining_request_budget() / SHADOW_MAX_REQUESTS_PER_TARGET;
        match trigger {
            ShadowTrigger::Webhook => {
                let scopes = std::mem::take(&mut self.pending_webhooks);
                self.webhook_due_at = None;
                self.webhook_first_at = None;
                let mut matched = Vec::new();
                let mut delayed_until = None::<Instant>;
                for target in targets
                    .iter()
                    .filter(|target| scopes.iter().any(|scope| scope.matches(target)))
                {
                    let key = (target.repo.clone(), target.pr, target.head_sha.clone());
                    if let Some(last) = self.target_cooldowns.get(&key)
                        && now.duration_since(*last) < SHADOW_TARGET_COOLDOWN
                    {
                        self.pending_webhooks.insert(ShadowWebhookScope {
                            repo: target.repo.clone(),
                            pr: Some(target.pr),
                            head_sha: Some(target.head_sha.clone()),
                        });
                        let due = *last + SHADOW_TARGET_COOLDOWN;
                        delayed_until = Some(delayed_until.map_or(due, |current| current.min(due)));
                    } else {
                        matched.push(target.clone());
                    }
                }
                let selection_limit = SHADOW_WEBHOOK_BUDGET.min(request_bounded_targets);
                let overflowed = matched.len() > selection_limit;
                for target in matched.iter().skip(selection_limit) {
                    self.pending_webhooks.insert(ShadowWebhookScope {
                        repo: target.repo.clone(),
                        pr: Some(target.pr),
                        head_sha: Some(target.head_sha.clone()),
                    });
                }
                if !self.pending_webhooks.is_empty() {
                    self.webhook_first_at = Some(now);
                    let overflow_due = overflowed.then_some(now + SHADOW_WEBHOOK_DEBOUNCE);
                    self.webhook_due_at = delayed_until.into_iter().chain(overflow_due).min();
                }
                let selected = matched
                    .into_iter()
                    .take(selection_limit)
                    .collect::<Vec<_>>();
                for target in &selected {
                    self.target_cooldowns.insert(
                        (target.repo.clone(), target.pr, target.head_sha.clone()),
                        now,
                    );
                }
                self.target_cooldowns
                    .retain(|_, last| now.duration_since(*last) < SHADOW_TARGET_COOLDOWN);
                selected
            }
            ShadowTrigger::PeriodicCatchUp => {
                self.next_catch_up_at = now + SHADOW_CATCH_UP_INTERVAL;
                if targets.is_empty() {
                    return Vec::new();
                }
                let count = targets
                    .len()
                    .min(SHADOW_CATCH_UP_BUDGET)
                    .min(request_bounded_targets);
                let selected = (0..count)
                    .map(|offset| targets[(self.periodic_cursor + offset) % targets.len()].clone())
                    .collect::<Vec<_>>();
                self.periodic_cursor = (self.periodic_cursor + count) % targets.len();
                for target in &selected {
                    self.target_cooldowns.insert(
                        (target.repo.clone(), target.pr, target.head_sha.clone()),
                        now,
                    );
                }
                selected
            }
        }
    }

    /// Finish a pass and return transition-only evidence.
    pub fn finish_pass(
        &mut self,
        report: &ShadowObservationReport,
    ) -> Vec<ShadowObservationTransition> {
        self.finish_pass_at(report, Instant::now())
    }

    fn retain_targets(&mut self, targets: &[ShadowPrTarget]) {
        let live = targets
            .iter()
            .map(|target| (target.repo.clone(), target.pr, target.head_sha.clone()))
            .collect::<BTreeSet<_>>();
        self.snapshots.retain(|key, _| live.contains(key));
        self.failed_targets.retain(|key, _| live.contains(key));
        self.target_cooldowns.retain(|key, _| live.contains(key));
    }

    fn remaining_request_budget(&self) -> usize {
        SHADOW_HOURLY_API_CEILING.saturating_sub(
            self.api_window
                .iter()
                .map(|(_, count)| count)
                .sum::<usize>(),
        )
    }

    fn finish_pass_at(
        &mut self,
        report: &ShadowObservationReport,
        now: Instant,
    ) -> Vec<ShadowObservationTransition> {
        self.in_flight = false;
        if report.api_requests > 0 {
            self.api_window.push_back((now, report.api_requests));
        }
        let mut transitions = Vec::new();
        for observation in &report.observations {
            let key = (
                observation.repo.clone(),
                observation.pr,
                observation.expected_head_sha.clone(),
            );
            let recovered = self.failed_targets.remove(&key).is_some();
            match self
                .snapshots
                .insert(key.clone(), observation.snapshot_digest.clone())
            {
                previous if recovered => transitions.push(ShadowObservationTransition {
                    kind: ShadowObservationTransitionKind::FetchRecovered,
                    repo: key.0,
                    pr: key.1,
                    expected_head_sha: key.2,
                    policy_revision: observation.policy_revision,
                    observation: Some(observation.clone()),
                    previous_snapshot_digest: previous,
                    failure_class: None,
                }),
                Some(previous) if previous != observation.snapshot_digest => {
                    transitions.push(ShadowObservationTransition {
                        kind: ShadowObservationTransitionKind::SnapshotChanged,
                        repo: key.0,
                        pr: key.1,
                        expected_head_sha: key.2,
                        policy_revision: observation.policy_revision,
                        observation: Some(observation.clone()),
                        previous_snapshot_digest: Some(previous),
                        failure_class: None,
                    });
                }
                _ => {}
            }
        }
        for failure in &report.failures {
            let key = (
                failure.repo.clone(),
                failure.pr,
                failure.expected_head_sha.clone(),
            );
            let class_changed = self
                .failed_targets
                .insert(
                    key.clone(),
                    (failure.failure_class.clone(), failure.policy_revision),
                )
                .as_ref()
                != Some(&(failure.failure_class.clone(), failure.policy_revision));
            if class_changed {
                let previous_snapshot_digest = self.snapshots.get(&key).cloned();
                transitions.push(ShadowObservationTransition {
                    kind: ShadowObservationTransitionKind::FetchFailed,
                    repo: key.0,
                    pr: key.1,
                    expected_head_sha: key.2,
                    policy_revision: failure.policy_revision,
                    observation: None,
                    previous_snapshot_digest,
                    failure_class: Some(failure.failure_class.clone()),
                });
            }
        }
        transitions
    }
}

impl ShadowWebhookScope {
    fn matches(&self, target: &ShadowPrTarget) -> bool {
        self.repo == target.repo
            && self.pr.is_none_or(|pr| pr == target.pr)
            && self
                .head_sha
                .as_deref()
                .is_none_or(|head| head == target.head_sha)
    }
}

/// Extract only webhook families capable of changing a PR/check snapshot.
#[must_use]
pub fn webhook_scope(event: &Value) -> Option<ShadowWebhookScope> {
    let kind = event.get("kind")?.as_str()?;
    if !matches!(
        kind,
        "workflow_run" | "pull_request" | "check_run" | "check_suite"
    ) {
        return None;
    }
    let payload = event.get("payload")?;
    let repo = payload.get("repo")?.as_str()?.trim().to_ascii_lowercase();
    if repo.split('/').count() != 2 {
        return None;
    }
    let pr = payload.get("number").and_then(Value::as_u64).or_else(|| {
        payload
            .get("pull_request_numbers")
            .and_then(Value::as_array)
            .and_then(|values| (values.len() == 1).then(|| values[0].as_u64()).flatten())
    });
    let head_sha = payload
        .get("head_sha")
        .and_then(Value::as_str)
        .filter(|head| head.len() == 40 || head.len() == 64)
        .map(str::to_owned);
    if pr.is_none() && head_sha.is_none() {
        return None;
    }
    Some(ShadowWebhookScope { repo, pr, head_sha })
}

/// Observe selected targets through the configured read-only GitHub boundary.
#[must_use]
pub fn observe_targets(
    _mode: RuntimeMode,
    global_dir: &Path,
    cwd: &Path,
    trigger: ShadowTrigger,
    targets: &[ShadowPrTarget],
) -> ShadowObservationReport {
    let started = Instant::now();
    let config = match LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf()) {
        Ok(config) => config,
        Err(error) => {
            let results = targets
                .iter()
                .map(|_| ShadowFetchResult {
                    result: Err(ReconcileFetchError::Prepare(format!(
                        "failed to load daemon GitHub auth config: {error}"
                    ))),
                    api_requests: 0,
                })
                .collect();
            return observation_report(trigger, targets, results, started.elapsed());
        }
    };
    if config.get_str("github.auth.source") != Some("command") {
        let results = targets
            .iter()
            .map(|_| ShadowFetchResult {
                result: Err(ReconcileFetchError::Prepare(
                    "shadow observation requires machine-global command auth".to_owned(),
                )),
                api_requests: 0,
            })
            .collect();
        return observation_report(trigger, targets, results, started.elapsed());
    }
    let gh_client = match GhClient::from_loaded_config(&config) {
        Ok(client) => client,
        Err(error) => {
            let results = targets
                .iter()
                .map(|_| ShadowFetchResult {
                    result: Err(ReconcileFetchError::Prepare(error.to_string())),
                    api_requests: 0,
                })
                .collect();
            return observation_report(trigger, targets, results, started.elapsed());
        }
    };
    let mut results = Vec::with_capacity(targets.len());
    for chunk in targets.chunks(SHADOW_FETCH_CONCURRENCY) {
        let chunk_results = thread::scope(|scope| {
            let handles = chunk
                .iter()
                .map(|target| {
                    scope.spawn(|| {
                        fetch_head_and_provenanced_status_check_rollup_for_repo_with_client(
                            &gh_client,
                            cwd,
                            &target.repo,
                            target.pr,
                            SHADOW_AUTH_TIMEOUT,
                            SHADOW_PASS_TIMEOUT.saturating_sub(started.elapsed()),
                        )
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| match handle.join() {
                    Ok(Ok(rollup)) => ShadowFetchResult {
                        result: Ok((rollup.head_sha, rollup.checks)),
                        api_requests: rollup.api_requests,
                    },
                    Ok(Err(ProvenancedFetchError {
                        error,
                        api_requests,
                    })) => ShadowFetchResult {
                        result: Err(error),
                        api_requests,
                    },
                    Err(_) => ShadowFetchResult {
                        result: Err(ReconcileFetchError::Io(
                            "shadow observation worker panicked".to_owned(),
                        )),
                        api_requests: 0,
                    },
                })
                .collect::<Vec<_>>()
        });
        results.extend(chunk_results);
    }
    observation_report(trigger, targets, results, started.elapsed())
}

/// Observe selected targets with an injected read boundary for focused tests.
#[must_use]
pub fn observe_targets_with<F>(
    trigger: ShadowTrigger,
    targets: &[ShadowPrTarget],
    mut fetch: F,
) -> ShadowObservationReport
where
    F: FnMut(&ShadowPrTarget) -> Result<(String, Vec<Value>), ReconcileFetchError>,
{
    let started = Instant::now();
    let results = targets
        .iter()
        .map(|target| {
            let result = fetch(target);
            let api_requests = usize::from(!matches!(
                result,
                Err(ReconcileFetchError::Prepare(_) | ReconcileFetchError::Spawn(_))
            ));
            ShadowFetchResult {
                result,
                api_requests,
            }
        })
        .collect::<Vec<_>>();
    observation_report(trigger, targets, results, started.elapsed())
}

struct ShadowFetchResult {
    result: Result<(String, Vec<Value>), ReconcileFetchError>,
    api_requests: usize,
}

fn observation_report(
    trigger: ShadowTrigger,
    targets: &[ShadowPrTarget],
    results: Vec<ShadowFetchResult>,
    elapsed: Duration,
) -> ShadowObservationReport {
    let api_requests = results.iter().map(|result| result.api_requests).sum();
    let mut observations = Vec::new();
    let mut failures = Vec::new();
    for (target, fetch) in targets.iter().zip(results) {
        match fetch.result {
            Ok((observed_head, checks)) => {
                observations.push(observation(target, observed_head, &checks));
            }
            Err(error) => failures.push(ShadowFetchFailure {
                repo: target.repo.clone(),
                pr: target.pr,
                expected_head_sha: target.head_sha.clone(),
                policy_revision: target.policy.revision,
                failure_class: fetch_failure_class(&error).to_owned(),
            }),
        }
    }
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    ShadowObservationReport {
        trigger,
        selected_targets: targets.len(),
        api_requests,
        observations,
        fetch_errors: failures.len(),
        failures,
        observer_failure_class: None,
        elapsed_ms,
        activation_enabled: false,
        dispatch_enabled: false,
        model_calls: 0,
    }
}

fn empty_report(trigger: ShadowTrigger) -> ShadowObservationReport {
    ShadowObservationReport {
        trigger,
        selected_targets: 0,
        api_requests: 0,
        observations: Vec::new(),
        failures: Vec::new(),
        fetch_errors: 0,
        observer_failure_class: None,
        elapsed_ms: 0,
        activation_enabled: false,
        dispatch_enabled: false,
        model_calls: 0,
    }
}

pub(crate) fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn prune_budget_entries(entries: &mut Vec<ShadowBudgetEntry>, now: u64) {
    entries.retain(|entry| now.saturating_sub(entry.epoch_seconds) < 3_600);
}

fn reported_rolling_requests(entries: &[ShadowBudgetEntry], persistence_failed: bool) -> usize {
    let accounted = entries
        .iter()
        .map(|entry| entry.api_requests)
        .sum::<usize>();
    if persistence_failed {
        accounted.max(SHADOW_HOURLY_API_CEILING)
    } else {
        accounted
    }
}

fn load_request_budget(path: &Path) -> Result<Vec<ShadowBudgetEntry>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut entries = serde_json::from_slice::<Vec<ShadowBudgetEntry>>(&bytes)
        .map_err(|error| error.to_string())?;
    prune_budget_entries(&mut entries, epoch_seconds());
    Ok(entries)
}

fn save_request_budget(path: &Path, entries: &[ShadowBudgetEntry]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "shadow budget path has no parent".to_owned())?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    serde_json::to_writer(temporary.as_file_mut(), entries).map_err(|error| error.to_string())?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| error.to_string())?;
    temporary.persist(path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn worker_panic_report(
    trigger: ShadowTrigger,
    targets: &[ShadowPrTarget],
) -> ShadowObservationReport {
    let mut report = observation_report(
        trigger,
        targets,
        targets
            .iter()
            .map(|_| ShadowFetchResult {
                result: Err(ReconcileFetchError::Io(
                    "shadow observation pass panicked".to_owned(),
                )),
                api_requests: 0,
            })
            .collect(),
        Duration::ZERO,
    );
    report.observer_failure_class = Some("worker_panic".to_owned());
    report
}

fn fetch_failure_class(error: &ReconcileFetchError) -> &'static str {
    match error {
        ReconcileFetchError::Spawn(_) => "spawn",
        ReconcileFetchError::Io(_) => "io",
        ReconcileFetchError::Timeout(_) => "timeout",
        ReconcileFetchError::Command(_) => "command",
        ReconcileFetchError::Parse(_) => "parse",
        ReconcileFetchError::Prepare(_) => "prepare",
    }
}

fn observation(
    target: &ShadowPrTarget,
    observed_head_sha: String,
    checks: &[Value],
) -> ShadowObservation {
    let mut normalized = checks
        .iter()
        .map(|check| {
            (
                check
                    .get("__typename")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                check
                    .get("name")
                    .or_else(|| check.get("context"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                check
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                check
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                check
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                check_producer_identity(check),
            )
        })
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    let (mut pending_checks, mut passed_checks, mut failed_checks) = check_counts(&normalized);
    let exact_head = observed_head_sha == target.head_sha;
    if !exact_head {
        pending_checks = 0;
        passed_checks = 0;
        failed_checks = 0;
    }
    let mut ledger_hasher = Sha256::new();
    ledger_hasher.update(target.repo.as_bytes());
    ledger_hasher.update(target.pr.to_le_bytes());
    ledger_hasher.update(target.head_sha.as_bytes());
    ledger_hasher.update(target.work_items.to_le_bytes());
    ledger_hasher.update(target.policy.revision.to_le_bytes());
    ledger_hasher.update(target.policy.primary_platform.as_bytes());
    ledger_hasher.update(target.policy.compatibility_mode.as_bytes());
    ledger_hasher.update(target.policy.blocking_rule.as_bytes());
    for lane in &target.policy.compatibility_lanes {
        ledger_hasher.update(lane.as_bytes());
        ledger_hasher.update([0]);
    }
    for lane in &target.policy.declared_dependency_lanes {
        ledger_hasher.update(lane.as_bytes());
        ledger_hasher.update([0]);
    }
    let ledger_digest = hex::encode(ledger_hasher.finalize());
    let mut github_hasher = Sha256::new();
    github_hasher.update(observed_head_sha.as_bytes());
    for fields in &normalized {
        for field in [
            fields.0,
            fields.1,
            fields.2,
            fields.3,
            fields.4,
            fields.5.as_str(),
        ] {
            github_hasher.update(field.as_bytes());
            github_hasher.update([0]);
        }
    }
    let github_digest = hex::encode(github_hasher.finalize());
    let mut combined_hasher = Sha256::new();
    combined_hasher.update(ledger_digest.as_bytes());
    combined_hasher.update(github_digest.as_bytes());
    ShadowObservation {
        repo: target.repo.clone(),
        pr: target.pr,
        expected_head_sha: target.head_sha.clone(),
        work_items: target.work_items,
        exact_head,
        observed_head_sha,
        snapshot_digest: hex::encode(combined_hasher.finalize()),
        ledger_digest,
        github_digest,
        pending_checks,
        passed_checks,
        failed_checks,
        policy_revision: target.policy.revision,
        primary_platform: target.policy.primary_platform.clone(),
        compatibility_mode: target.policy.compatibility_mode.clone(),
        blocking_rule: target.policy.blocking_rule.clone(),
    }
}

fn check_counts(checks: &[(&str, &str, &str, &str, &str, String)]) -> (u64, u64, u64) {
    let mut pending = 0;
    let mut passed = 0;
    let mut failed = 0;
    for (_, _, status, conclusion, state, _) in checks {
        let status = status.to_ascii_uppercase();
        let state = state.to_ascii_uppercase();
        let conclusion = conclusion.to_ascii_uppercase();
        if matches!(
            status.as_str(),
            "QUEUED" | "PENDING" | "IN_PROGRESS" | "REQUESTED" | "WAITING"
        ) || matches!(
            state.as_str(),
            "EXPECTED" | "QUEUED" | "PENDING" | "IN_PROGRESS"
        ) {
            pending += 1;
        } else if matches!(conclusion.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED")
            || state == "SUCCESS"
        {
            passed += 1;
        } else if !conclusion.is_empty() || matches!(state.as_str(), "ERROR" | "FAILURE") {
            failed += 1;
        }
    }
    (pending, passed, failed)
}

fn check_producer_identity(check: &Value) -> String {
    match check.get("__typename").and_then(Value::as_str) {
        Some("CheckRun") => {
            let app = check.pointer("/checkSuite/app");
            serde_json::json!([
                "app",
                app.and_then(|value| value.get("databaseId")),
                app.and_then(|value| value.get("slug")),
            ])
            .to_string()
        }
        Some("StatusContext") => {
            let creator = check.get("creator");
            serde_json::json!([
                "creator",
                creator.and_then(|value| value.get("__typename")),
                creator.and_then(|value| value.get("databaseId")),
                creator.and_then(|value| value.get("login")),
            ])
            .to_string()
        }
        _ => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_ledger::RepoPolicy;

    fn target(repo: &str, pr: u64, head: char) -> ShadowPrTarget {
        ShadowPrTarget {
            repo: repo.to_owned(),
            pr,
            head_sha: head.to_string().repeat(40),
            work_items: 1,
            policy: RepoPolicy {
                repo: repo.to_owned(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                revision: 1,
            },
        }
    }

    #[test]
    fn periodic_catch_up_is_subscriber_free_bounded_and_round_robin() {
        let now = Instant::now();
        let mut scheduler = ShadowScheduler::new(now);
        let targets = (1..=12)
            .map(|pr| target("generous-corp/pulp", pr, 'a'))
            .collect::<Vec<_>>();

        assert_eq!(
            scheduler.due_trigger(now),
            Some(ShadowTrigger::PeriodicCatchUp)
        );
        let first = scheduler.begin_pass(ShadowTrigger::PeriodicCatchUp, &targets, now);
        assert_eq!(first.len(), SHADOW_CATCH_UP_BUDGET);
        assert_eq!(first[0].pr, 1);
        let empty = observe_targets_with(ShadowTrigger::PeriodicCatchUp, &[], |_| unreachable!());
        scheduler.finish_pass(&empty);

        let later = now + SHADOW_CATCH_UP_INTERVAL;
        let second = scheduler.begin_pass(ShadowTrigger::PeriodicCatchUp, &targets, later);
        assert_eq!(second[0].pr, 9);
        assert_eq!(second.last().map(|entry| entry.pr), Some(4));
    }

    #[test]
    fn webhook_burst_debounces_and_selects_only_exact_matching_pr() {
        let now = Instant::now();
        let mut scheduler = ShadowScheduler::new(now);
        // Consume the initially due catch-up so only the webhook deadline matters.
        let _ = scheduler.begin_pass(ShadowTrigger::PeriodicCatchUp, &[], now);
        let empty = observe_targets_with(ShadowTrigger::PeriodicCatchUp, &[], |_| unreachable!());
        scheduler.finish_pass(&empty);
        let event = serde_json::json!({
            "kind": "check_run",
            "payload": {
                "repo": "generous-corp/pulp",
                "pull_request_numbers": [42],
                "head_sha": "b".repeat(40)
            }
        });
        scheduler.note_webhook(&event, now);
        scheduler.note_webhook(&event, now + Duration::from_millis(500));

        assert_eq!(scheduler.due_trigger(now + Duration::from_secs(2)), None);
        let due = now + Duration::from_millis(2_500);
        assert_eq!(scheduler.due_trigger(due), Some(ShadowTrigger::Webhook));
        let selected = scheduler.begin_pass(
            ShadowTrigger::Webhook,
            &[
                target("generous-corp/pulp", 41, 'a'),
                target("generous-corp/pulp", 42, 'b'),
                target("generous-corp/forge", 42, 'c'),
            ],
            due,
        );
        assert_eq!(selected, vec![target("generous-corp/pulp", 42, 'b')]);

        scheduler.finish_pass(&empty_report(ShadowTrigger::Webhook));
        let stale = serde_json::json!({
            "kind": "check_run",
            "payload": {
                "repo": "generous-corp/pulp",
                "pull_request_numbers": [42],
                "head_sha": "c".repeat(40)
            }
        });
        scheduler.note_webhook(&stale, due);
        assert!(
            scheduler
                .begin_pass(
                    ShadowTrigger::Webhook,
                    &[target("generous-corp/pulp", 42, 'b')],
                    due + SHADOW_WEBHOOK_DEBOUNCE,
                )
                .is_empty()
        );
    }

    #[test]
    fn webhook_traffic_cannot_postpone_periodic_catch_up() {
        let now = Instant::now();
        let mut scheduler = ShadowScheduler::new(now);
        let event = serde_json::json!({
            "kind": "pull_request",
            "payload": {"repo": "generous-corp/pulp", "number": 42}
        });
        scheduler.note_webhook(&event, now);
        assert_eq!(
            scheduler.due_trigger(now),
            Some(ShadowTrigger::PeriodicCatchUp)
        );
        let _ = scheduler.begin_pass(ShadowTrigger::PeriodicCatchUp, &[], now);
        scheduler.finish_pass(&empty_report(ShadowTrigger::PeriodicCatchUp));

        let webhook_at = now + Duration::from_mins(4);
        scheduler.note_webhook(&event, webhook_at);
        let webhook_due = webhook_at + SHADOW_WEBHOOK_DEBOUNCE;
        assert_eq!(
            scheduler.due_trigger(webhook_due),
            Some(ShadowTrigger::Webhook)
        );
        let _ = scheduler.begin_pass(ShadowTrigger::Webhook, &[], webhook_due);
        scheduler.finish_pass(&empty_report(ShadowTrigger::Webhook));

        scheduler.note_webhook(&event, now + SHADOW_CATCH_UP_INTERVAL);
        assert_eq!(
            scheduler.due_trigger(now + SHADOW_CATCH_UP_INTERVAL),
            Some(ShadowTrigger::PeriodicCatchUp)
        );
    }

    #[test]
    fn observations_are_read_only_zero_model_and_transition_only() {
        let expected = target("generous-corp/vellum", 8, 'a');
        let checks = vec![serde_json::json!({
            "name": "macOS",
            "status": "COMPLETED",
            "conclusion": "SUCCESS"
        })];
        let report = observe_targets_with(
            ShadowTrigger::Webhook,
            std::slice::from_ref(&expected),
            |_| Ok((expected.head_sha.clone(), checks.clone())),
        );
        assert_eq!(report.api_requests, 1);
        assert_eq!(report.model_calls, 0);
        assert!(!report.activation_enabled);
        assert!(!report.dispatch_enabled);
        assert!(report.observations[0].exact_head);
        assert_eq!(report.observations[0].passed_checks, 1);
        assert_eq!(report.observations[0].primary_platform, "macos");
        assert_eq!(report.observations[0].compatibility_mode, "independent");

        let mut scheduler = ShadowScheduler::new(Instant::now());
        assert!(
            scheduler.finish_pass(&report).is_empty(),
            "baseline is not logged"
        );
        assert!(
            scheduler.finish_pass(&report).is_empty(),
            "unchanged is suppressed"
        );

        let changed = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&expected),
            |_| {
                Ok((
                    expected.head_sha.clone(),
                    vec![serde_json::json!({
                        "name": "macOS",
                        "status": "COMPLETED",
                        "conclusion": "FAILURE"
                    })],
                ))
            },
        );
        let transitions = scheduler.finish_pass(&changed);
        assert_eq!(transitions.len(), 1);
        assert_eq!(
            transitions[0].kind,
            ShadowObservationTransitionKind::SnapshotChanged
        );
        assert_eq!(
            transitions[0]
                .observation
                .as_ref()
                .map(|observation| observation.failed_checks),
            Some(1)
        );
        assert_eq!(
            transitions[0].previous_snapshot_digest.as_deref(),
            Some(report.observations[0].snapshot_digest.as_str())
        );
    }

    #[test]
    fn fetch_failure_and_recovery_emit_once_without_raw_error_detail() {
        let expected = target("generous-corp/pulp", 99, 'a');
        let failed = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&expected),
            |_| {
                Err(ReconcileFetchError::Command(
                    "secret-bearing stderr".to_owned(),
                ))
            },
        );
        let mut scheduler = ShadowScheduler::new(Instant::now());

        let first = scheduler.finish_pass(&failed);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].kind, ShadowObservationTransitionKind::FetchFailed);
        assert_eq!(first[0].failure_class.as_deref(), Some("command"));
        assert!(
            !serde_json::to_string(&first)
                .expect("json")
                .contains("secret-bearing")
        );
        assert!(scheduler.finish_pass(&failed).is_empty());

        let mut revised = expected.clone();
        revised.policy.revision = 2;
        let revised_failure = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&revised),
            |_| Err(ReconcileFetchError::Command("same class".to_owned())),
        );
        let policy_transition = scheduler.finish_pass(&revised_failure);
        assert_eq!(policy_transition.len(), 1);
        assert_eq!(policy_transition[0].policy_revision, 2);

        let prepare_failure = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&expected),
            |_| Err(ReconcileFetchError::Prepare("auth unavailable".to_owned())),
        );
        assert_eq!(prepare_failure.api_requests, 0);

        let changed_failure = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&expected),
            |_| Err(ReconcileFetchError::Timeout("timed out".to_owned())),
        );
        let changed = scheduler.finish_pass(&changed_failure);
        assert_eq!(changed.len(), 1);
        assert_eq!(
            changed[0].kind,
            ShadowObservationTransitionKind::FetchFailed
        );
        assert_eq!(changed[0].failure_class.as_deref(), Some("timeout"));

        let recovered = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&expected),
            |_| Ok((expected.head_sha.clone(), Vec::new())),
        );
        let transitions = scheduler.finish_pass(&recovered);
        assert_eq!(transitions.len(), 1);
        assert_eq!(
            transitions[0].kind,
            ShadowObservationTransitionKind::FetchRecovered
        );
        assert!(transitions[0].observation.is_some());
    }

    #[test]
    fn exact_head_drift_is_evidence_not_a_mutation_or_retry() {
        let expected = target("generous-corp/forge", 148, 'a');
        let report = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&expected),
            |_| {
                Ok((
                    "b".repeat(40),
                    vec![serde_json::json!({
                        "status": "COMPLETED",
                        "conclusion": "SUCCESS"
                    })],
                ))
            },
        );

        assert_eq!(report.api_requests, 1);
        assert_eq!(report.observations.len(), 1);
        assert!(!report.observations[0].exact_head);
        assert_eq!(report.observations[0].passed_checks, 0);
        assert_ne!(
            report.observations[0].ledger_digest,
            report.observations[0].github_digest
        );
        assert_eq!(report.model_calls, 0);
        assert!(!report.dispatch_enabled);
    }

    #[test]
    fn status_context_identity_prevents_cross_context_state_swaps_from_hiding() {
        let expected = target("generous-corp/pulp", 100, 'a');
        let initial = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&expected),
            |_| {
                Ok((
                    expected.head_sha.clone(),
                    vec![
                        serde_json::json!({
                            "__typename": "StatusContext",
                            "context": "macos",
                            "state": "SUCCESS"
                        }),
                        serde_json::json!({
                            "__typename": "StatusContext",
                            "context": "legacy",
                            "state": "FAILURE"
                        }),
                    ],
                ))
            },
        );
        let swapped = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&expected),
            |_| {
                Ok((
                    expected.head_sha.clone(),
                    vec![
                        serde_json::json!({
                            "__typename": "StatusContext",
                            "context": "macos",
                            "state": "FAILURE"
                        }),
                        serde_json::json!({
                            "__typename": "StatusContext",
                            "context": "legacy",
                            "state": "SUCCESS"
                        }),
                    ],
                ))
            },
        );

        assert_ne!(
            initial.observations[0].snapshot_digest,
            swapped.observations[0].snapshot_digest
        );
    }

    #[test]
    fn producer_identity_and_complete_status_states_are_preserved() {
        let expected = target("generous-corp/pulp", 101, 'a');
        let snapshot = |first_state: &str, second_state: &str| {
            observe_targets_with(
                ShadowTrigger::PeriodicCatchUp,
                std::slice::from_ref(&expected),
                |_| {
                    Ok((
                        expected.head_sha.clone(),
                        vec![
                            serde_json::json!({
                                "__typename": "CheckRun",
                                "name": "build",
                                "status": "COMPLETED",
                                "conclusion": first_state,
                                "checkSuite": {"app": {"databaseId": 1, "slug": "one"}}
                            }),
                            serde_json::json!({
                                "__typename": "CheckRun",
                                "name": "build",
                                "status": "COMPLETED",
                                "conclusion": second_state,
                                "checkSuite": {"app": {"databaseId": 2, "slug": "two"}}
                            }),
                            serde_json::json!({
                                "__typename": "StatusContext",
                                "context": "queued",
                                "status": "REQUESTED",
                                "state": "EXPECTED",
                                "creator": {"__typename": "Bot", "databaseId": 3, "login": "bot"}
                            }),
                            serde_json::json!({
                                "__typename": "StatusContext",
                                "context": "broken",
                                "status": "WAITING",
                                "state": "ERROR",
                                "creator": {"__typename": "Bot", "databaseId": 3, "login": "bot"}
                            }),
                        ],
                    ))
                },
            )
        };
        let initial = snapshot("SUCCESS", "FAILURE");
        let swapped = snapshot("FAILURE", "SUCCESS");

        assert_eq!(initial.observations[0].pending_checks, 2);
        assert_eq!(initial.observations[0].failed_checks, 1);
        assert_ne!(
            initial.observations[0].snapshot_digest,
            swapped.observations[0].snapshot_digest
        );
    }

    #[test]
    fn webhook_coalescing_has_a_maximum_age_and_requeues_overflow() {
        let now = Instant::now();
        let mut scheduler = ShadowScheduler::new(now);
        let _ = scheduler.begin_pass(ShadowTrigger::PeriodicCatchUp, &[], now);
        scheduler.finish_pass_at(&empty_report(ShadowTrigger::PeriodicCatchUp), now);
        for second in 0..20 {
            scheduler.note_webhook(
                &serde_json::json!({
                    "kind": "pull_request",
                    "payload": {"repo": "generous-corp/pulp", "number": second + 1}
                }),
                now + Duration::from_secs(second),
            );
        }
        assert_eq!(
            scheduler.due_trigger(now + SHADOW_WEBHOOK_MAX_COALESCE),
            Some(ShadowTrigger::Webhook)
        );
        let targets = (1..=20)
            .map(|pr| target("generous-corp/pulp", pr, 'a'))
            .collect::<Vec<_>>();
        let selected = scheduler.begin_pass(
            ShadowTrigger::Webhook,
            &targets,
            now + SHADOW_WEBHOOK_MAX_COALESCE,
        );
        assert_eq!(selected.len(), SHADOW_WEBHOOK_BUDGET);
        scheduler.finish_pass_at(
            &empty_report(ShadowTrigger::Webhook),
            now + SHADOW_WEBHOOK_MAX_COALESCE,
        );
        assert_eq!(
            scheduler.due_trigger(now + SHADOW_WEBHOOK_MAX_COALESCE + SHADOW_WEBHOOK_DEBOUNCE),
            Some(ShadowTrigger::Webhook)
        );
    }

    #[test]
    fn webhook_arriving_during_cooldown_is_delayed_not_dropped() {
        let now = Instant::now();
        let expected = target("generous-corp/pulp", 42, 'a');
        let event = serde_json::json!({
            "kind": "pull_request",
            "payload": {"repo": "generous-corp/pulp", "number": 42}
        });
        let mut scheduler = ShadowScheduler::new(now);
        let _ = scheduler.begin_pass(ShadowTrigger::PeriodicCatchUp, &[], now);
        scheduler.finish_pass_at(&empty_report(ShadowTrigger::PeriodicCatchUp), now);
        scheduler.note_webhook(&event, now);
        let first_due = now + SHADOW_WEBHOOK_DEBOUNCE;
        assert_eq!(
            scheduler
                .begin_pass(
                    ShadowTrigger::Webhook,
                    std::slice::from_ref(&expected),
                    first_due
                )
                .len(),
            1
        );
        scheduler.finish_pass_at(&empty_report(ShadowTrigger::Webhook), first_due);
        scheduler.note_webhook(&event, first_due + Duration::from_secs(1));
        let retry = first_due + SHADOW_TARGET_COOLDOWN;
        assert!(
            scheduler
                .begin_pass(
                    ShadowTrigger::Webhook,
                    std::slice::from_ref(&expected),
                    first_due + SHADOW_WEBHOOK_DEBOUNCE,
                )
                .is_empty()
        );
        scheduler.finish_pass_at(
            &empty_report(ShadowTrigger::Webhook),
            first_due + SHADOW_WEBHOOK_DEBOUNCE,
        );
        assert_eq!(scheduler.due_trigger(retry), Some(ShadowTrigger::Webhook));
    }

    #[test]
    fn workflow_job_without_exact_pr_identity_is_not_accelerated() {
        assert!(
            webhook_scope(&serde_json::json!({
                "kind": "workflow_job",
                "payload": {"repo": "generous-corp/pulp"}
            }))
            .is_none()
        );
    }

    #[test]
    fn api_accounting_distinguishes_pre_spawn_from_post_spawn_io() {
        let expected = target("generous-corp/pulp", 102, 'a');
        let spawn = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&expected),
            |_| Err(ReconcileFetchError::Spawn("missing gh".to_owned())),
        );
        let io = observe_targets_with(
            ShadowTrigger::PeriodicCatchUp,
            std::slice::from_ref(&expected),
            |_| Err(ReconcileFetchError::Io("capture failed".to_owned())),
        );
        assert_eq!(spawn.api_requests, 0);
        assert_eq!(io.api_requests, 1);
    }

    #[test]
    fn hourly_ceiling_reserves_worst_case_pagination_before_selection() {
        let now = Instant::now();
        let mut scheduler = ShadowScheduler::new(now);
        scheduler.api_window.push_back((now, 239));
        assert_eq!(scheduler.due_trigger(now), None);

        scheduler.api_window.clear();
        scheduler.api_window.push_back((now, 220));
        assert_eq!(
            scheduler.due_trigger(now),
            Some(ShadowTrigger::PeriodicCatchUp)
        );
        let targets = (1..=8)
            .map(|pr| target("generous-corp/pulp", pr, 'a'))
            .collect::<Vec<_>>();
        assert_eq!(
            scheduler
                .begin_pass(ShadowTrigger::PeriodicCatchUp, &targets, now)
                .len(),
            2
        );

        scheduler.api_window.clear();
        scheduler.api_window.push_back((now, 231));
        assert_eq!(
            scheduler.next_due_after(now),
            Duration::from_hours(1),
            "status due time must include the budget-release deadline"
        );

        scheduler.api_window.clear();
        scheduler.api_window.push_back((
            now.checked_sub(Duration::from_mins(30))
                .expect("test instant has history"),
            5,
        ));
        scheduler.api_window.push_back((now, 230));
        assert_eq!(
            scheduler.next_due_after(now),
            Duration::from_mins(30),
            "the earliest expiring charge should reopen one target slot"
        );
    }

    #[test]
    fn failed_budget_persistence_reports_the_conservative_effective_charge() {
        let entries = vec![ShadowBudgetEntry {
            epoch_seconds: epoch_seconds(),
            api_requests: 3,
        }];
        assert_eq!(reported_rolling_requests(&entries, false), 3);
        assert_eq!(
            reported_rolling_requests(&entries, true),
            SHADOW_HOURLY_API_CEILING
        );
    }

    #[test]
    fn request_budget_survives_restart_and_prunes_expired_entries() {
        let state = tempfile::tempdir().expect("state");
        let path = state.path().join("shadow-observer-budget.json");
        let now = epoch_seconds();
        save_request_budget(
            &path,
            &[
                ShadowBudgetEntry {
                    epoch_seconds: now,
                    api_requests: 17,
                },
                ShadowBudgetEntry {
                    epoch_seconds: now.saturating_sub(3_601),
                    api_requests: 99,
                },
            ],
        )
        .expect("save");
        let restored = load_request_budget(&path).expect("load");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].api_requests, 17);
    }

    #[test]
    fn in_flight_reservation_is_durable_then_reconciled_to_actual_cost() {
        let state = tempfile::tempdir().expect("state");
        let now = Instant::now();
        let mut lane = ShadowDaemonLane::new(
            RuntimeMode::Shipyard,
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            now,
        );
        lane.reserve_requests(80, now).expect("reserve");
        assert_eq!(
            load_request_budget(&lane.budget_path)
                .expect("reserved budget")
                .iter()
                .map(|entry| entry.api_requests)
                .sum::<usize>(),
            80
        );
        let mut report = empty_report(ShadowTrigger::PeriodicCatchUp);
        report.api_requests = 3;
        lane.finish_report(&report, now);
        assert_eq!(
            load_request_budget(&lane.budget_path)
                .expect("actual budget")
                .iter()
                .map(|entry| entry.api_requests)
                .sum::<usize>(),
            3
        );
        let health = lane.health.lock().expect("health").clone();
        assert_eq!(health.reserved_requests, 0);
        assert_eq!(health.last_reserved_requests, 80);
        assert_eq!(health.last_actual_requests, 3);
        assert_eq!(health.rolling_hour_requests, 3);
    }

    #[test]
    fn webhook_deadline_is_published_without_changing_unrelated_event_status() {
        let state = tempfile::tempdir().expect("state");
        let now = Instant::now();
        let mut lane = ShadowDaemonLane::new(
            RuntimeMode::Shipyard,
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            now,
        );
        assert!(lane.tick(now).is_empty());
        let periodic_due = lane
            .health
            .lock()
            .expect("health")
            .next_due_at
            .expect("periodic due");

        lane.note_webhook(
            &serde_json::json!({
                "kind": "check_run",
                "payload": {
                    "repo": "generous-corp/pulp",
                    "pull_request_numbers": [42],
                    "head_sha": "b".repeat(40)
                }
            }),
            now + Duration::from_secs(1),
        );
        let webhook_due = lane
            .health
            .lock()
            .expect("health")
            .next_due_at
            .expect("webhook due");
        assert!(webhook_due < periodic_due);
        assert!(webhook_due <= epoch_seconds().saturating_add(3));

        let before_unrelated = lane.health.lock().expect("health").clone();
        lane.note_webhook(
            &serde_json::json!({"kind": "installation", "payload": {}}),
            now + Duration::from_secs(2),
        );
        assert_eq!(
            *lane.health.lock().expect("health"),
            before_unrelated,
            "unrelated webhook stays status-silent"
        );
    }

    #[test]
    fn durable_reservation_budget_gates_published_next_due() {
        let state = tempfile::tempdir().expect("state");
        let wall_now = epoch_seconds();
        save_request_budget(
            &state.path().join("shadow-observer-budget.json"),
            &[ShadowBudgetEntry {
                epoch_seconds: wall_now,
                api_requests: 160,
            }],
        )
        .expect("existing budget");
        let now = Instant::now();
        let mut lane = ShadowDaemonLane::new(
            RuntimeMode::Shipyard,
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            now,
        );

        lane.reserve_requests(80, now).expect("reserve");
        let health = lane.health.lock().expect("health").clone();
        assert_eq!(health.reserved_requests, 80);
        assert_eq!(health.rolling_hour_requests, 240);
        assert!(
            health.next_due_at.expect("budget release due")
                >= wall_now.saturating_add(Duration::from_hours(1).as_secs() - 1)
        );
    }

    #[test]
    fn tick_ages_expired_request_charges_out_of_durable_health() {
        let state = tempfile::tempdir().expect("state");
        let now = Instant::now();
        let mut lane = ShadowDaemonLane::new(
            RuntimeMode::Shipyard,
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            now,
        );
        let expired_at = epoch_seconds().saturating_sub(3_601);
        lane.budget_entries.push(ShadowBudgetEntry {
            epoch_seconds: expired_at,
            api_requests: 17,
        });
        save_request_budget(&lane.budget_path, &lane.budget_entries).expect("expired budget");
        lane.scheduler.api_window.push_back((
            now.checked_sub(Duration::from_secs(3_601))
                .expect("test instant has history"),
            17,
        ));
        lane.scheduler.next_catch_up_at = now + SHADOW_CATCH_UP_INTERVAL;
        lane.update_health(|health| health.rolling_hour_requests = 17);

        assert!(lane.tick(now).is_empty());
        assert_eq!(lane.health.lock().expect("health").rolling_hour_requests, 0);
        assert!(
            load_request_budget(&lane.budget_path)
                .expect("pruned budget")
                .is_empty()
        );
    }

    #[test]
    fn failed_reservation_reports_and_expires_conservative_scheduler_charge() {
        let state = tempfile::tempdir().expect("state");
        let now = Instant::now();
        let mut lane = ShadowDaemonLane::new(
            RuntimeMode::Shipyard,
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            now,
        );
        lane.budget_path = state.path().join("missing-parent/budget.json");
        assert!(lane.reserve_requests(80, now).is_err());
        lane.scheduler
            .api_window
            .push_back((now, SHADOW_HOURLY_API_CEILING));
        let mut report = empty_report(ShadowTrigger::PeriodicCatchUp);
        report.observer_failure_class = Some("budget_reservation".to_owned());
        lane.finish_report(&report, now);
        assert_eq!(
            lane.health.lock().expect("health").rolling_hour_requests,
            SHADOW_HOURLY_API_CEILING
        );

        lane.scheduler.next_catch_up_at = now + Duration::from_hours(2);
        assert!(
            lane.tick(now + Duration::from_hours(1) + Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(lane.health.lock().expect("health").rolling_hour_requests, 0);
    }

    #[test]
    fn ledger_failure_publishes_retry_gate_instead_of_past_due_schedule() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        let length = fs::metadata(ledger.path()).expect("ledger metadata").len();
        fs::OpenOptions::new()
            .write(true)
            .open(ledger.path())
            .expect("open ledger")
            .set_len(length / 2)
            .expect("truncate ledger");
        let now = Instant::now();
        let wall_now = epoch_seconds();
        let mut lane = ShadowDaemonLane::new(
            RuntimeMode::Shipyard,
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            now,
        );

        assert!(lane.tick(now).is_empty());
        let first_due = lane
            .health
            .lock()
            .expect("health")
            .next_due_at
            .expect("retry due");
        assert!(first_due >= wall_now.saturating_add(4));
        assert_eq!(
            lane.health
                .lock()
                .expect("health")
                .last_failure_class
                .as_deref(),
            Some("ledger")
        );

        assert!(lane.tick(now + Duration::from_secs(1)).is_empty());
        let gated_due = lane
            .health
            .lock()
            .expect("health")
            .next_due_at
            .expect("gated retry due");
        assert!(gated_due <= first_due);
        assert!(gated_due >= epoch_seconds().saturating_add(3));
    }

    #[test]
    fn quiet_success_health_is_durable_and_restart_visible() {
        let state = tempfile::tempdir().expect("state");
        let now = Instant::now();
        let mut lane = ShadowDaemonLane::new(
            RuntimeMode::Shipyard,
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            now,
        );

        assert!(lane.tick(now).is_empty(), "empty baseline stays silent");
        let before_restart = lane.health.lock().expect("health").clone();
        assert!(before_restart.last_success_at.is_some());
        assert_eq!(before_restart.exact_target_count, 0);
        assert_eq!(before_restart.in_flight_since, None);
        assert!(!before_restart.activation_enabled);
        assert!(!before_restart.dispatch_enabled);
        assert_eq!(before_restart.model_calls, 0);
        assert_eq!(
            before_restart.status_value(epoch_seconds())["stalled"],
            false
        );

        let restarted = ShadowDaemonLane::new(
            RuntimeMode::Shipyard,
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            now + Duration::from_secs(1),
        );
        let after_restart = restarted.health.lock().expect("health").clone();
        assert_eq!(
            after_restart.last_success_at,
            before_restart.last_success_at
        );
        assert_eq!(after_restart.last_actual_requests, 0);
        assert!(after_restart.next_due_at.is_some());
    }

    #[test]
    fn stalled_pass_is_visible_and_restart_becomes_a_durable_failure() {
        let state = tempfile::tempdir().expect("state");
        let path = state.path().join("shadow-observer-health.json");
        let now = epoch_seconds();
        let mut health = ShadowObserverHealth::default();
        health.in_flight_since = Some(now.saturating_sub(SHADOW_PASS_TIMEOUT.as_secs() + 1));
        health.reserved_requests = 20;
        health.periodic_cursor = 7;
        save_observer_health(&path, &health).expect("health receipt");
        assert_eq!(health.status_value(now)["stalled"], true);

        let restarted = ShadowDaemonLane::new(
            RuntimeMode::Shipyard,
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            state.path().to_path_buf(),
            Instant::now(),
        );
        let recovered = restarted.health.lock().expect("health").clone();
        assert_eq!(recovered.in_flight_since, None);
        assert_eq!(recovered.reserved_requests, 0);
        assert_eq!(restarted.scheduler.periodic_cursor, 7);
        assert_eq!(
            recovered.last_failure_class.as_deref(),
            Some("daemon_restarted_during_pass")
        );
        assert_eq!(
            load_observer_health(&path)
                .expect("restart-visible health")
                .last_failure_class
                .as_deref(),
            Some("daemon_restarted_during_pass")
        );
    }
}
